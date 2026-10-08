//! PhotoCraft on Android.
//!
//! Runs the same [`photocraft_ui_egui::PhotocraftApp`] as the desktop app inside a
//! `GameActivity` (android-activity's `game-activity` backend, which eframe needs for the soft
//! keyboard and accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then
//! packaged by the Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no TCP control server;
//! - File › Open asks `MainActivity.pickOpen()` (Storage Access Framework); the bytes come back
//!   on a Java thread through `nativeDeliverFile` into `Services::inbox`, like the web build;
//! - Save and Export write to `Downloads/PhotoCraft/<name>` through `MainActivity.saveToDownloads`
//!   (MediaStore, no dialog), and saving the same name again overwrites that file;
//! - preferences and the panel layout live in the app's private files directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use android_activity::AndroidApp;
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};
use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_doc::Document;
use photocraft_engine::Session;
use photocraft_ui_egui::theme::ThemeKind;
use photocraft_ui_egui::{Inbox, PhotocraftApp, Services};

const LOG_TAG: &str = "photocraft";

/// Files the Kotlin side delivers (name, bytes); the app opens them on its next frame.
static INBOX: OnceLock<Inbox> = OnceLock::new();
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

fn inbox() -> &'static Inbox {
    INBOX.get_or_init(Inbox::default)
}

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    log::info!("PhotoCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "PhotoCraft",
        options,
        Box::new(move |cc| {
            PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = PhotocraftApp::new(Session::new(), services(&data_dir));
            app.set_theme(&cc.egui_ctx, ThemeKind::Pro);
            // Long commands and file opens run as background jobs with progress and Cancel.
            app.background_jobs = true;
            if let Some(rs) = cc.wgpu_render_state.clone() {
                let info = rs.adapter.get_info();
                log::info!("wgpu backend {:?}, adapter {}", info.backend, info.name);
                app.perf.gpu_info.set_adapter(&info);
                app.set_wgpu(rs);
            }
            Ok(Box::new(app))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("PhotoCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// Everything File › Open reads (the same list as the web build; the picker itself shows all
/// files, since Android has no MIME type for PSD or PhotoCraft documents).
fn services(data_dir: &Path) -> Services {
    let prefs = data_dir.join("preferences.json");
    let prefs_save = prefs.clone();
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8]| photocraft_io::import(name, bytes).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string()))),
        export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
            let mut opts = photocraft_io::ExportOptions::default();
            if let Some(q) = settings.jpeg_quality {
                opts.encode.jpeg_quality = q;
            }
            opts.encode.webp_lossless = settings.webp_lossless;
            if let Some(q) = settings.webp_quality {
                opts.encode.webp_quality = q;
            }
            opts.tiff_layers = settings.tiff_layers;
            opts.xmp = if settings.xmp_all { photocraft_io::XmpEmbed::All } else { photocraft_io::XmpEmbed::None };
            photocraft_io::export(doc, path, &opts).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        // The picker is asynchronous: the file arrives later through the inbox.
        pick_open: Some(Box::new(|| {
            if let Err(e) = pick_open() {
                log::error!("couldn't open the file picker: {e}");
            }
            None
        })),
        // No save dialog: the suggested name becomes the file name in Downloads/PhotoCraft.
        pick_save: Some(Box::new(|suggested: &str| Some(file_name(suggested)))),
        write: Some(Box::new(|path: &str, bytes: &[u8]| save_to_downloads(&file_name(path), bytes))),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        inbox: Some(inbox().clone()),
        open_url: Some(Box::new(|url: &str| open_url(url))),
        load_prefs: Some(Box::new(move || std::fs::read_to_string(&prefs).ok())),
        save_prefs: Some(Box::new(move |text: &str| write_atomic(&prefs_save, text.as_bytes()))),
        ..Default::default()
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.saveToDownloads(name, bytes)`: `null` on success, else the error message.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let ret = env
            .call_method(activity, jni_str!("saveToDownloads"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jname).into(), (&jbytes).into()])?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

/// `MainActivity.openUrl(url)`: Help menu links in the browser.
fn open_url(url: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jurl = JString::from_str(env, url)?;
        env.call_method(activity, jni_str!("openUrl"), jni_sig!("(Ljava/lang/String;)V"), &[(&jurl).into()])?;
        Ok(())
    })
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, bytes)`: a picked file's contents, from a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_photocraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        log::info!("received {name} ({} bytes)", bytes.len());
        inbox().lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
