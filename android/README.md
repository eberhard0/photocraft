# PhotoCraft for Android

The Gradle project that packages `apps/photocraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/photocraft-android/src/lib.rs`: `android_main`, the platform services (open, save,
  preferences, URLs) and the JNI bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker, saving into
  `Downloads/PhotoCraft/`, and the full-screen window.
- `cargo ndk` drops `libphotocraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p photocraft-android --features heif
cd android && ./gradlew assembleDebug
```

## Keyboard shortcuts

A Bluetooth or USB keyboard works like on the desktop: the same Photoshop shortcut table
(`crates/ui-egui/src/menu_catalog.rs`, tool keys in `state.rs`, held keys in `hold_keys.rs`,
and Edit › Keyboard Shortcuts… overrides) is dispatched by `shortcut_dispatch.rs`. Cmd in the
table means Ctrl on Android.

## Known limits (first version)

- Save and Export write to `Downloads/PhotoCraft/<name>` without a dialog; saving the same name
  again in one session overwrites it.
- No crash-recovery autosave yet, no clipboard images, no Open Recent.
- The desktop layout needs a tablet-sized screen; on a phone's cover screen it is cramped.
