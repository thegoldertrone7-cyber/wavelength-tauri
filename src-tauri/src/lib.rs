pub mod api;
pub mod audio;
pub mod bridge;
pub mod desktop;
pub mod diag;
pub mod generator;
pub mod lyrics;
pub mod store;
pub mod updater;

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const PORT: u16 = 47823;

fn public_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("public"))
        .unwrap_or_else(|| PathBuf::from("public"))
}

fn data_dir() -> PathBuf {
    std::env::var("WL_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| store::default_data_dir())
}

fn boot(
    dir: PathBuf,
    public: PathBuf,
    shell: Option<(tauri::AppHandle, bool)>,
) -> Result<(), String> {
    let store = store::Store::open(&dir)?;
    // An explicit WL_DATA_DIR is a test or a second profile: keep it isolated
    // instead of adopting the real library from another folder.
    if std::env::var_os("WL_DATA_DIR").is_none() {
        if let Some(from) = store.recover_library() {
            println!("restored the library from {}", from.display());
        }
    }
    let state = Arc::new(api::AppState::new(store));
    api::resume_sync_host(state.clone());
    if let Some((handle, updater_ready)) = shell {
        state.updater.attach(handle, updater_ready);
    }
    let app = api::router(state, public.clone());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT))
            .await
            .map_err(|e| format!("bind 127.0.0.1:{PORT}: {e}"))?;
        println!("wavelength core on http://127.0.0.1:{PORT} (public: {})", public.display());
        axum::serve(listener, app).await.map_err(|e| e.to_string())
    })
}

pub fn run() {
    diag::install_panic_hook(data_dir());
    // WebKitGTK's DMA-BUF renderer is the usual cause of blank windows and web-process crashes on Linux (NVIDIA,
    // Wayland, some Mesa builds). Turning it off costs little. An explicit setting from the user always wins.
    #[cfg(target_os = "linux")]
    {
        for (k, v) in [("WEBKIT_DISABLE_DMABUF_RENDERER", "1")] {
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
        // GPU compositing in WebKitGTK is the other common reason the web process dies (the window goes blank or the
        // app closes the moment a long, image-heavy page such as a playlist is drawn). Software compositing is
        // stable everywhere and this interface is plain enough that it stays smooth. WL_GPU=1 turns acceleration
        // back on for anyone whose drivers are fine; WL_SAFE_RENDER is kept for compatibility.
        if std::env::var_os("WL_GPU").is_none() && std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
    }
    // WebView2 slows timers in a minimised or covered window, which delays end-of-song detection and the media
    // buttons. Wavelength plays through its own engine, so the page's timers have to keep their pace.
    #[cfg(windows)]
    {
        if std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none() {
            std::env::set_var(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows",
            );
        }
    }
    let headless = std::env::args().any(|a| a == "--headless")
        || std::env::var("WL_HEADLESS").is_ok();

    if headless {
        if let Err(e) = boot(data_dir(), public_dir(), None) {
            eprintln!("wavelength core: {e}");
            std::process::exit(1);
        }
        return;
    }

    let dev_public = public_dir();
    let dev_data = data_dir();

    // The updater plugin refuses to start without a public key, so it is only
    // registered when tauri.conf.json has one (release builds). A local build
    // without a key simply reports "Updates are not set up in this build".
    let context = tauri::generate_context!();
    let updater_ready = updater::configured(context.config());
    let mut builder = tauri::Builder::default();
    if updater_ready {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }

    builder
        .setup(move |app| {
            use tauri::Manager;
            let data = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| dev_data.clone());
            let resource_public = app
                .path()
                .resource_dir()
                .ok()
                .map(|r| r.join("public"))
                .filter(|p| p.join("index.html").is_file());
            let public = resource_public.unwrap_or_else(|| dev_public.clone());

            let public_for_server = public.clone();
            let handle = app.handle().clone();
            let shell = (app.handle().clone(), updater_ready);
            std::thread::spawn(move || {
                if let Err(e) = boot(data, public_for_server, Some(shell)) {
                    eprintln!("wavelength core: {e}");
                    handle.exit(1);
                }
            });

            // Wait until the core is listening, then create the window so the
            // first load never races the server.
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(8) {
                if TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(60));
            }

            let url = format!("http://127.0.0.1:{PORT}");
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse().expect("valid url")),
            )
            .title("Wavelength")
            .inner_size(1280.0, 840.0)
            .min_inner_size(900.0, 620.0)
            .resizable(true)
            // The app is dark by default; without this the window flashes white before the page paints, and the
            // title bar stays light. The page corrects the frame to match its theme once it loads.
            .theme(Some(tauri::Theme::Dark))
            .background_color(tauri::window::Color(11, 11, 12, 255))
            .build()?;
            #[cfg(windows)]
            let hwnd = window.hwnd().ok().map(|h| h.0 as *mut std::ffi::c_void);
            #[cfg(not(windows))]
            let hwnd: Option<*mut std::ffi::c_void> = None;
            desktop::init(app.handle().clone(), hwnd);
            Ok(())
        })
        .run(context)
        .expect("error while running Wavelength");
}
