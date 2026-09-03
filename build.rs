fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // `set_resource_file` (previously used here) hands the compiler an
        // already-written .rc file *instead of* winres's generated one — see
        // winres::WindowsResource::set_resource_file: "replaces the internally
        // generated resource file". assets/rampp.rc only ever set the icon, so
        // the compiled resource carried no VERSIONINFO block. Calling
        // `set_icon_with_id` instead lets winres emit its own .rc containing
        // both the icon — kept under the name "icon" (not the default numeric
        // id "1") because src/tray.rs looks it up at runtime via
        // `IconSource::Resource("icon")`, which resolves by resource *name* —
        // and a VERSIONINFO block seeded from CARGO_PKG_VERSION
        // (FILEVERSION/PRODUCTVERSION) automatically.
        let mut res = winres::WindowsResource::new();
        res.set_icon_with_id("assets/icon.ico", "icon");
        res.compile().expect("failed to compile Windows resources");

        // winres's own `compile()` prints `cargo:rustc-link-lib=dylib=resource`
        // to tell rustc to link the compiled resource (OUT_DIR/resource.lib,
        // actually raw .res output from rc.exe) into the crate being built.
        // That directive only reaches the *library* target of this package —
        // this package also has a `main.rs` binary target of the same name,
        // and a `cargo:rustc-link-lib` from a package's own build script does
        // not propagate to that package's own binary targets (verified
        // empirically: built rampp.exe carried zero RT_VERSION/RT_ICON/
        // RT_GROUP_ICON resources despite `resource.lib` compiling cleanly).
        // `rustc-link-arg-bin` instead attaches the same file directly to the
        // `rampp` binary's own link line, which does reach the final exe.
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let resource_lib = std::path::Path::new(&out_dir).join("resource.lib");
        println!("cargo:rustc-link-arg-bin=rampp={}", resource_lib.display());
    }
}
