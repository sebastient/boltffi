use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::build::{BindingExpansion, BuildOptions, BuildSelection, Builder};
use crate::cli::{CliError, Result};
use crate::config::Config;
use crate::pack::PackError;
use crate::pack::java::link::link_search_path_flags;
use crate::pack::print_cargo_line;
use crate::pack::symbols::{DebugSymbolArtifact, DebugSymbolArtifactKind, write_debug_symbols_zip};
use crate::target::{BuiltLibrary, Platform, RustTarget};
use crate::toolchain::AndroidToolchain;

use super::AndroidBindingMode;

/// Native libraries already hardcoded into [`android_shared_link_args`]'s baseline. Flags
/// reported by `cargo rustc --print=native-static-libs` that exactly match one of these are
/// dropped so the final linker invocation doesn't pass true duplicates.
const HARDCODED_ANDROID_BASELINE_LIBS: [&str; 4] = ["-lc", "-lm", "-llog", "-ldl"];

/// Optional cargo build context that lets [`AndroidPackager`] ask Cargo (via
/// `cargo rustc --print=native-static-libs`) what native libraries the staticlib actually
/// requires, so those libraries can be threaded into the manual `clang -shared` link step
/// alongside the historical hardcoded baseline. See [`AndroidPackager::with_build_context`].
#[derive(Debug, Clone, Default)]
struct AndroidBuildContext {
    binding_expansion: Option<BindingExpansion>,
    cargo_args: Vec<String>,
}

pub struct AndroidPackager<'a> {
    config: &'a Config,
    libraries: Vec<BuiltLibrary>,
    release: bool,
    binding_mode: AndroidBindingMode,
    layout: AndroidPackageLayout,
    build_context: Option<AndroidBuildContext>,
}

/// Paths used while compiling and staging Android JNI libraries.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct AndroidPackageLayout {
    pub(crate) jni_glue_path: PathBuf,
    pub(crate) header_include_dir: PathBuf,
    pub(crate) header_name: String,
    pub(crate) jnilibs_path: PathBuf,
}

pub struct AndroidOutput;

struct AndroidLinkedOutput {
    target: RustTarget,
    abi: &'static str,
    path: PathBuf,
}

impl<'a> AndroidPackager<'a> {
    /// Builds an Android packager for the standalone Kotlin Android output layout.
    pub fn new(config: &'a Config, libraries: Vec<BuiltLibrary>, release: bool) -> Self {
        let layout = AndroidPackageLayout::kotlin(config);
        Self::new_with_layout(
            config,
            libraries,
            release,
            AndroidBindingMode::Kotlin,
            layout,
        )
    }

    /// Builds an Android packager with an explicit layout supplied by the owning package flow.
    pub(crate) fn new_with_layout(
        config: &'a Config,
        libraries: Vec<BuiltLibrary>,
        release: bool,
        binding_mode: AndroidBindingMode,
        layout: AndroidPackageLayout,
    ) -> Self {
        Self {
            config,
            libraries,
            release,
            binding_mode,
            layout,
            build_context: None,
        }
    }

    /// Supplies the cargo build context (a resolved [`BindingExpansion`] and/or pass-through
    /// cargo args) needed to query `cargo rustc --print=native-static-libs` for each Android
    /// target during linking, so additional native libraries required by the crate graph
    /// (e.g. `-lnativewindow`) reach the final `.so` link instead of only the hardcoded
    /// baseline (`-lm -llog -ldl`, plus the implicit `-lc`).
    ///
    /// This is opt-in: without calling this, [`AndroidPackager`] behaves exactly as before
    /// (hardcoded baseline libs only).
    pub fn with_build_context(
        mut self,
        binding_expansion: Option<BindingExpansion>,
        cargo_args: Vec<String>,
    ) -> Self {
        self.build_context = Some(AndroidBuildContext {
            binding_expansion,
            cargo_args,
        });
        self
    }

    pub fn package(self) -> Result<AndroidOutput> {
        let android_libs = self.filter_android_libraries();

        if android_libs.is_empty() {
            return Err(PackError::NoLibrariesFound {
                platform: "Android".to_string(),
            }
            .into());
        }

        let jnilibs_path = self.android_jnilibs_path();
        let android_toolchain = AndroidToolchain::discover(
            self.config.android_min_sdk(),
            self.config.android_ndk_version(),
        )?;

        std::fs::create_dir_all(&jnilibs_path).map_err(|source| {
            CliError::CreateDirectoryFailed {
                path: jnilibs_path.clone(),
                source,
            }
        })?;

        let jni_glue_path = self.android_jni_glue_path()?;
        let header_include_dir = self.layout.header_include_dir.clone();
        let header_path = header_include_dir.join(format!("{}.h", self.layout.header_name));
        if !header_path.exists() {
            return Err(CliError::FileNotFound(header_path));
        }

        let mut linked_outputs = Vec::with_capacity(android_libs.len());
        for lib in &android_libs {
            linked_outputs.push(self.link_shared_library(
                lib,
                &jnilibs_path,
                &android_toolchain,
                &jni_glue_path,
                &header_include_dir,
            )?);
        }

        if self.android_debug_symbols_archive_enabled() {
            write_android_debug_symbols(self.config, &linked_outputs)?;
        }

        self.remove_stale_packaged_libraries(&jnilibs_path, &android_libs)?;

        Ok(AndroidOutput)
    }

    fn remove_stale_packaged_libraries(
        &self,
        jnilibs_path: &Path,
        android_libs: &[&BuiltLibrary],
    ) -> Result<()> {
        let packaged_triples: std::collections::HashSet<_> = android_libs
            .iter()
            .map(|library| library.target.triple())
            .collect();
        let lib_file_name = format!("lib{}.so", self.android_library_name());

        for target in RustTarget::ALL_ANDROID {
            if packaged_triples.contains(target.triple()) {
                continue;
            }

            let stale_output = jnilibs_path
                .join(target.architecture().android_abi())
                .join(&lib_file_name);
            if stale_output.exists() {
                std::fs::remove_file(&stale_output).map_err(|source| CliError::CommandFailed {
                    command: format!("remove stale android library {}", stale_output.display()),
                    status: source.raw_os_error(),
                })?;
            }
        }

        Ok(())
    }

    fn filter_android_libraries(&self) -> Vec<&BuiltLibrary> {
        self.libraries
            .iter()
            .filter(|lib| lib.target.platform() == Platform::Android)
            .collect()
    }

    fn android_jnilibs_path(&self) -> PathBuf {
        self.layout.jnilibs_path.clone()
    }

    fn android_jni_glue_path(&self) -> Result<PathBuf> {
        let jni_glue_path = self.layout.jni_glue_path.clone();
        jni_glue_path
            .exists()
            .then_some(jni_glue_path.clone())
            .ok_or(CliError::FileNotFound(jni_glue_path))
    }

    fn link_shared_library(
        &self,
        library: &BuiltLibrary,
        jnilibs_path: &Path,
        android_toolchain: &AndroidToolchain,
        jni_glue_path: &Path,
        header_include_dir: &Path,
    ) -> Result<AndroidLinkedOutput> {
        let abi = library.target.architecture().android_abi();
        let abi_dir = jnilibs_path.join(abi);

        std::fs::create_dir_all(&abi_dir).map_err(|source| CliError::CreateDirectoryFailed {
            path: abi_dir.clone(),
            source,
        })?;

        let lib_name = self.android_library_name();
        let dest_path = abi_dir.join(format!("lib{}.so", lib_name));
        let build_dir = PathBuf::from("target")
            .join("boltffi")
            .join("android")
            .join(library.target.triple())
            .join(if self.release { "release" } else { "debug" });
        std::fs::create_dir_all(&build_dir).map_err(|source| CliError::CreateDirectoryFailed {
            path: build_dir.clone(),
            source,
        })?;

        let clang = android_toolchain.clang_for_target(&library.target)?;
        let object_path = build_dir.join("jni_glue.o");
        let export_script_path = build_dir.join("exports.map");

        let mut compile = Command::new(&clang);
        compile.args(android_jni_compile_args(
            &object_path,
            header_include_dir,
            jni_glue_path,
            self.release,
            self.android_debug_symbols_enabled(),
        ));
        run_command(compile)?;

        write_android_export_version_script(&export_script_path)?;

        let extra_link_flags = self.extra_link_flags(&library.target, android_toolchain)?;

        let mut link = Command::new(&clang);
        link.args(android_shared_link_args(
            &dest_path,
            &object_path,
            &library.path,
            &export_script_path,
            &extra_link_flags,
        ));
        run_command(link)?;

        Ok(AndroidLinkedOutput {
            target: library.target,
            abi,
            path: dest_path,
        })
    }

    /// Resolves the extra `-L`/`-l...` flags that should be threaded into the manual
    /// `clang -shared` link step, on top of the hardcoded baseline in
    /// [`android_shared_link_args`].
    ///
    /// Returns an empty list when no build context was supplied via
    /// [`Self::with_build_context`] (the pre-existing, hardcoded-only behavior).
    ///
    /// If a build context was supplied but the `cargo rustc --print=native-static-libs`
    /// query itself fails to run (e.g. an NDK/toolchain issue, or an unresolved `cargo`
    /// invocation while offline), this deliberately does NOT fail the whole `pack android`
    /// command: it warns and falls back to the hardcoded baseline instead, so this feature
    /// stays a strict improvement and never becomes a new failure mode on its own. Unlike
    /// that subprocess-level failure, a *genuinely* missing native library is still caught
    /// -- at actual link time, as a hard build failure -- by `-Wl,--no-undefined` in
    /// [`android_shared_link_args`], which is the more appropriate place to fail loudly
    /// since a missing symbol here would otherwise ship a `.so` that crashes silently on
    /// device.
    fn extra_link_flags(
        &self,
        target: &RustTarget,
        android_toolchain: &AndroidToolchain,
    ) -> Result<Vec<String>> {
        let Some(build_context) = &self.build_context else {
            return Ok(Vec::new());
        };

        let selection = match &build_context.binding_expansion {
            Some(binding_expansion) => {
                BuildSelection::Expanded(Box::new(binding_expansion.clone()))
            }
            None => BuildSelection::Default {
                cargo_args: build_context.cargo_args.clone(),
            },
        };
        let builder = Builder::new(
            self.config,
            BuildOptions {
                release: self.release,
                selection,
                on_output: None,
            },
        );

        match builder.query_native_static_libs(target, android_toolchain) {
            Ok(metadata) => {
                let mut flags = link_search_path_flags(&metadata.native_link_search_paths);
                flags.extend(filter_redundant_hardcoded_libs(
                    &metadata.native_static_libraries,
                ));
                Ok(flags)
            }
            Err(error) => {
                print_cargo_line(&format!(
                    "warning: failed to query native-static-libs for android target '{}': {error}; \
                     falling back to the hardcoded baseline native libraries (-lm -llog -ldl)",
                    target.triple()
                ));
                Ok(Vec::new())
            }
        }
    }

    fn android_library_name(&self) -> String {
        self.config.resolved_android_kotlin_library_name()
    }

    fn android_debug_symbols_enabled(&self) -> bool {
        matches!(self.binding_mode, AndroidBindingMode::Kotlin)
            && self.config.android_debug_symbols_enabled()
    }

    fn android_debug_symbols_archive_enabled(&self) -> bool {
        matches!(self.binding_mode, AndroidBindingMode::Kotlin)
            && self.config.android_debug_symbols_archive_enabled()
    }
}

impl AndroidPackageLayout {
    /// Builds the default layout for standalone Kotlin Android packaging.
    pub(crate) fn kotlin(config: &Config) -> Self {
        Self {
            jni_glue_path: config
                .android_kotlin_output()
                .join("jni")
                .join("jni_glue.c"),
            header_include_dir: config.android_header_output(),
            header_name: config.library_name().to_string(),
            jnilibs_path: config.android_pack_output(),
        }
    }
}

fn android_jni_compile_args(
    object_path: &Path,
    header_include_dir: &Path,
    jni_glue_path: &Path,
    release: bool,
    emit_debug_info: bool,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("-fPIC"),
        OsString::from(if release { "-O3" } else { "-O0" }),
    ];
    if emit_debug_info {
        args.push(OsString::from("-g"));
    }
    args.extend([
        OsString::from("-I"),
        header_include_dir.as_os_str().to_os_string(),
        jni_glue_path.as_os_str().to_os_string(),
        OsString::from("-o"),
        object_path.as_os_str().to_os_string(),
    ]);
    args
}

fn write_android_debug_symbols(
    config: &Config,
    linked_outputs: &[AndroidLinkedOutput],
) -> Result<PathBuf> {
    let artifacts = linked_outputs
        .iter()
        .map(|output| DebugSymbolArtifact {
            source_path: output.path.clone(),
            archive_path: PathBuf::from("jniLibs").join(output.abi).join(
                output
                    .path
                    .file_name()
                    .expect("android library should have a filename"),
            ),
            kind: DebugSymbolArtifactKind::Shared,
            target_triple: Some(output.target.triple().to_string()),
            platform: Some(output.target.platform()),
            architecture: Some(output.target.architecture()),
            abi: Some(output.abi.to_string()),
            host_target: None,
        })
        .collect::<Vec<_>>();

    write_debug_symbols_zip(
        &config.android_debug_symbols_output(),
        &match config.android_debug_symbols_format() {
            crate::config::DebugSymbolsFormat::Zip => {
                format!("{}.android.symbols.zip", config.crate_artifact_name())
            }
        },
        "android",
        match config.android_debug_symbols_bundle() {
            crate::config::DebugSymbolsBundle::Unstripped => "unstripped",
        },
        &artifacts,
    )
}

/// Removes flags that exactly duplicate [`HARDCODED_ANDROID_BASELINE_LIBS`] from a list of
/// native-static-libs flags reported by `cargo rustc --print=native-static-libs`, so the
/// final linker invocation doesn't pass true duplicates. Everything else (e.g.
/// `-lnativewindow`, `-landroid`, `-lgcc`, ...) passes through unchanged.
fn filter_redundant_hardcoded_libs(native_static_libraries: &[String]) -> Vec<String> {
    native_static_libraries
        .iter()
        .filter(|flag| !HARDCODED_ANDROID_BASELINE_LIBS.contains(&flag.as_str()))
        .cloned()
        .collect()
}

fn android_shared_link_args(
    dest_path: &Path,
    object_path: &Path,
    library_path: &Path,
    export_script_path: &Path,
    extra_link_flags: &[String],
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-shared"),
        OsString::from("-o"),
        dest_path.as_os_str().to_os_string(),
        object_path.as_os_str().to_os_string(),
        OsString::from("-Wl,--whole-archive"),
        library_path.as_os_str().to_os_string(),
        OsString::from("-Wl,--no-whole-archive"),
    ];
    // Additional `-L`/`-l...` flags Cargo reports as required by the crate graph (e.g.
    // `-lnativewindow`), on top of the hardcoded baseline below. These aren't part of the
    // `--whole-archive` block above, so their placement relative to it doesn't matter for
    // `-l` resolution.
    args.extend(extra_link_flags.iter().map(OsString::from));
    args.extend([
        OsString::from("-Xlinker"),
        OsString::from("--version-script"),
        OsString::from("-Xlinker"),
        export_script_path.as_os_str().to_os_string(),
        OsString::from("-Wl,--gc-sections"),
        OsString::from("-Wl,-z,nodelete"),
        // Fail the build loudly if any symbol -- including ones from a crate-declared
        // `extern "C"` FFI binding several dependency layers down -- is still undefined
        // after linking, instead of silently shipping a `.so` that crashes at runtime when
        // the dynamic linker can't resolve it on-device.
        OsString::from("-Wl,--no-undefined"),
        OsString::from("-lm"),
        OsString::from("-llog"),
        OsString::from("-ldl"),
    ]);
    args
}

fn write_android_export_version_script(path: &Path) -> Result<()> {
    std::fs::write(path, android_export_version_script()).map_err(|source| {
        CliError::CommandFailed {
            command: format!("write android linker version script {}", path.display()),
            status: source.raw_os_error(),
        }
    })
}

fn android_export_version_script() -> &'static str {
    r#"{
    global:
        Java_*;
        JNI_OnLoad*;
        JNI_OnUnload*;
        boltffi_*;
    local:
        *;
};
"#
}

fn run_command(mut command: Command) -> Result<()> {
    let command_string = format!("{:?}", command);
    let status = command.status().map_err(|_| CliError::CommandFailed {
        command: command_string.clone(),
        status: None,
    })?;

    status
        .success()
        .then_some(())
        .ok_or(CliError::CommandFailed {
            command: command_string,
            status: status.code(),
        })
}

#[cfg(test)]
mod tests {
    use super::{
        AndroidPackageLayout, AndroidPackager, android_export_version_script,
        android_jni_compile_args, android_shared_link_args, filter_redundant_hardcoded_libs,
    };
    use crate::config::Config;
    use crate::pack::android::AndroidBindingMode;
    use crate::target::{BuiltLibrary, RustTarget};
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse_config(input: &str) -> Config {
        let parsed: Config = toml::from_str(input).expect("toml parse failed");
        parsed.validate().expect("config validation failed");
        parsed
    }

    fn kmp_android_layout(output_root: &Path) -> AndroidPackageLayout {
        AndroidPackageLayout {
            jni_glue_path: output_root.join("src/androidMain/c/jni_glue.c"),
            header_include_dir: output_root.join("src/androidMain/c"),
            header_name: "demo".to_string(),
            jnilibs_path: output_root.join("src/androidMain/jniLibs"),
        }
    }

    #[test]
    fn stale_cleanup_removes_only_boltffi_library_file() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("boltffi-android-packager-test-{unique}"));
        let pack_output = root.join("jniLibs");
        let config = parse_config(&format!(
            r#"
[package]
name = "demo"

[targets.android.pack]
output = "{}"
"#,
            pack_output.display()
        ));

        let stale_abi_dir = pack_output.join("x86");
        fs::create_dir_all(&stale_abi_dir).expect("create stale abi dir");
        let stale_boltffi = stale_abi_dir.join("libdemo.so");
        let unrelated = stale_abi_dir.join("libdependency.so");
        fs::write(&stale_boltffi, []).expect("write stale boltffi lib");
        fs::write(&unrelated, []).expect("write unrelated lib");

        let arm64_abi_dir = pack_output.join("arm64-v8a");
        fs::create_dir_all(&arm64_abi_dir).expect("create configured abi dir");
        let packager = AndroidPackager::new(
            &config,
            vec![BuiltLibrary {
                target: RustTarget::ANDROID_ARM64,
                path: root.join("libdemo.a"),
            }],
            false,
        );
        let android_libs = packager.filter_android_libraries();

        packager
            .remove_stale_packaged_libraries(&pack_output, &android_libs)
            .expect("cleanup succeeds");

        assert!(!stale_boltffi.exists());
        assert!(unrelated.exists());

        fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn android_jni_glue_path_uses_kotlin_output_for_legacy_android_bindings() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("boltffi-android-jni-path-test-{unique}"));
        let kotlin_output = root.join("kotlin");
        let expected_glue = kotlin_output.join("jni/jni_glue.c");
        fs::create_dir_all(expected_glue.parent().expect("jni glue parent"))
            .expect("create kotlin jni dir");
        fs::write(&expected_glue, []).expect("write kotlin jni glue");

        let config = parse_config(&format!(
            r#"
[package]
name = "demo"

[targets.android.kotlin]
output = "{}"
"#,
            kotlin_output.display()
        ));
        let packager = AndroidPackager::new(&config, Vec::new(), false);

        assert_eq!(
            packager.android_jni_glue_path().expect("jni glue path"),
            expected_glue
        );

        fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn android_jnilibs_path_uses_android_pack_output_for_legacy_android_bindings_even_with_kmp() {
        let root = std::env::temp_dir().join("boltffi-android-jni-output-test");
        let pack_output = root.join("configured-jniLibs");
        let kmp_output = root.join("kmp");
        let config = parse_config(&format!(
            r#"
experimental = ["kotlin_multiplatform"]

[package]
name = "demo"

[targets.android.pack]
output = "{}"

[targets.kotlin_multiplatform]
enabled = true
output = "{}"
"#,
            pack_output.display(),
            kmp_output.display()
        ));
        let packager = AndroidPackager::new(&config, Vec::new(), false);

        assert_eq!(packager.android_jnilibs_path(), pack_output);
    }

    #[test]
    fn android_library_name_uses_kotlin_loader_override() {
        let config = parse_config(
            r#"
[package]
name = "demo"

[targets.android.kotlin]
library_name = "configured-library"
"#,
        );
        let legacy_packager = AndroidPackager::new(&config, Vec::new(), false);
        let kmp_packager = AndroidPackager::new_with_layout(
            &config,
            Vec::new(),
            false,
            AndroidBindingMode::KotlinMultiplatform,
            kmp_android_layout(Path::new("kmp")),
        );

        assert_eq!(legacy_packager.android_library_name(), "configured-library");
        assert_eq!(kmp_packager.android_library_name(), "configured-library");
    }

    #[test]
    fn android_jni_glue_path_uses_kmp_android_glue_for_kmp_bindings() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("boltffi-android-kmp-jni-path-test-{unique}"));
        let kmp_output = root.join("kmp");
        let expected_glue = kmp_output.join("src/androidMain/c/jni_glue.c");
        fs::create_dir_all(expected_glue.parent().expect("jni glue parent"))
            .expect("create kmp jni dir");
        fs::write(&expected_glue, []).expect("write kmp jni glue");

        let config = parse_config(&format!(
            r#"
experimental = ["kotlin_multiplatform"]

[package]
name = "demo"

[targets.kotlin_multiplatform]
enabled = true
output = "{}"
"#,
            kmp_output.display()
        ));
        let packager = AndroidPackager::new_with_layout(
            &config,
            Vec::new(),
            false,
            AndroidBindingMode::KotlinMultiplatform,
            kmp_android_layout(&kmp_output),
        );

        assert_eq!(
            packager.android_jni_glue_path().expect("jni glue path"),
            expected_glue
        );

        fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn android_jnilibs_path_uses_kmp_android_main_jnilibs_for_kmp_bindings() {
        let root = std::env::temp_dir().join("boltffi-android-kmp-jni-output-test");
        let kmp_output = root.join("kmp");
        let android_pack_output = root.join("legacy-jniLibs");
        let config = parse_config(&format!(
            r#"
experimental = ["kotlin_multiplatform"]

[package]
name = "demo"

[targets.android.pack]
output = "{}"

[targets.kotlin_multiplatform]
enabled = true
output = "{}"
"#,
            android_pack_output.display(),
            kmp_output.display()
        ));
        let packager = AndroidPackager::new_with_layout(
            &config,
            Vec::new(),
            false,
            AndroidBindingMode::KotlinMultiplatform,
            kmp_android_layout(&kmp_output),
        );

        assert_eq!(
            packager.android_jnilibs_path(),
            kmp_output.join("src/androidMain/jniLibs")
        );
    }

    #[test]
    fn kmp_android_packager_uses_explicit_layout_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("boltffi-android-kmp-explicit-layout-test-{unique}"));
        let config_kmp_output = root.join("config-kmp");
        let layout_kmp_output = root.join("layout-kmp");
        let expected_glue = layout_kmp_output.join("src/androidMain/c/jni_glue.c");
        let expected_jnilibs = layout_kmp_output.join("src/androidMain/jniLibs");
        fs::create_dir_all(expected_glue.parent().expect("jni glue parent"))
            .expect("create layout jni dir");
        fs::write(&expected_glue, []).expect("write layout jni glue");

        let config = parse_config(&format!(
            r#"
experimental = ["kotlin_multiplatform"]

[package]
name = "demo"

[targets.kotlin_multiplatform]
enabled = true
output = "{}"
"#,
            config_kmp_output.display()
        ));
        let layout = AndroidPackageLayout {
            jni_glue_path: expected_glue.clone(),
            header_include_dir: layout_kmp_output.join("src/androidMain/c"),
            header_name: "demo".to_string(),
            jnilibs_path: expected_jnilibs.clone(),
        };
        let packager = AndroidPackager::new_with_layout(
            &config,
            Vec::new(),
            false,
            AndroidBindingMode::KotlinMultiplatform,
            layout,
        );

        assert_eq!(
            packager.android_jni_glue_path().expect("jni glue path"),
            expected_glue
        );
        assert_eq!(packager.android_jnilibs_path(), expected_jnilibs);

        fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn kmp_android_packaging_does_not_write_legacy_debug_symbols() {
        let config = parse_config(
            r#"
[package]
name = "demo"

[targets.android.debug_symbols]
enabled = true
"#,
        );
        let legacy_packager = AndroidPackager::new(&config, Vec::new(), false);
        let kmp_packager = AndroidPackager::new_with_layout(
            &config,
            Vec::new(),
            false,
            AndroidBindingMode::KotlinMultiplatform,
            kmp_android_layout(Path::new("kmp")),
        );

        assert!(legacy_packager.android_debug_symbols_enabled());
        assert!(!kmp_packager.android_debug_symbols_enabled());
    }

    #[test]
    fn android_linker_uses_export_map_and_collects_unused_sections() {
        let args = android_shared_link_args(
            Path::new("/tmp/out/libdemo.so"),
            Path::new("/tmp/out/jni_glue.o"),
            Path::new("/tmp/out/libdemo.a"),
            Path::new("/tmp/out/exports.map"),
            &[],
        );

        assert!(!args.contains(&OsString::from("-Wl,--exclude-libs,ALL")));
        assert!(args.contains(&OsString::from("--version-script")));
        assert!(args.contains(&OsString::from("/tmp/out/exports.map")));
        assert!(args.contains(&OsString::from("-Wl,--gc-sections")));
    }

    #[test]
    fn android_linker_marks_jni_library_nodelete_for_cached_thread_destructors() {
        let args = android_shared_link_args(
            Path::new("/tmp/out/libdemo.so"),
            Path::new("/tmp/out/jni_glue.o"),
            Path::new("/tmp/out/libdemo.a"),
            Path::new("/tmp/out/exports.map"),
            &[],
        );

        assert!(
            args.contains(&OsString::from("-Wl,-z,nodelete")),
            "Android JNI libraries should stay mapped so pthread TLS destructors remain callable"
        );
    }

    #[test]
    fn android_linker_fails_loudly_on_undefined_symbols() {
        let args = android_shared_link_args(
            Path::new("/tmp/out/libdemo.so"),
            Path::new("/tmp/out/jni_glue.o"),
            Path::new("/tmp/out/libdemo.a"),
            Path::new("/tmp/out/exports.map"),
            &[],
        );

        assert!(
            args.contains(&OsString::from("-Wl,--no-undefined")),
            "a missing native lib must fail the build instead of shipping a broken .so"
        );
    }

    #[test]
    fn android_linker_includes_extra_link_flags_from_native_static_libs_query() {
        let extra_link_flags = vec![
            "-lnativewindow".to_string(),
            "-L/tmp/search-path".to_string(),
        ];
        let args = android_shared_link_args(
            Path::new("/tmp/out/libdemo.so"),
            Path::new("/tmp/out/jni_glue.o"),
            Path::new("/tmp/out/libdemo.a"),
            Path::new("/tmp/out/exports.map"),
            &extra_link_flags,
        );

        assert!(args.contains(&OsString::from("-lnativewindow")));
        assert!(args.contains(&OsString::from("-L/tmp/search-path")));
    }

    #[test]
    fn filter_redundant_hardcoded_libs_drops_only_exact_baseline_matches() {
        let reported = vec![
            "-lc".to_string(),
            "-lm".to_string(),
            "-lnativewindow".to_string(),
        ];

        assert_eq!(
            filter_redundant_hardcoded_libs(&reported),
            vec!["-lnativewindow".to_string()]
        );
    }

    #[test]
    fn android_export_map_keeps_public_jni_and_boltffi_symbols() {
        let script = android_export_version_script();

        assert!(script.contains("Java_*;"));
        assert!(script.contains("JNI_OnLoad*;"));
        assert!(script.contains("JNI_OnUnload*;"));
        assert!(script.contains("boltffi_*;"));
        assert!(script.contains("local:"));
        assert!(script.contains("*;"));
    }

    #[test]
    fn android_jni_compile_args_include_debug_info_when_requested() {
        let args = android_jni_compile_args(
            Path::new("/tmp/out/jni_glue.o"),
            Path::new("/tmp/include"),
            Path::new("/tmp/jni/jni_glue.c"),
            true,
            true,
        );

        assert!(args.contains(&OsString::from("-g")));
    }

    #[cfg(unix)]
    #[test]
    fn android_linker_preserves_non_utf8_paths() {
        let dest_path = PathBuf::from(OsString::from_vec(b"/tmp/out-\xFF.so".to_vec()));
        let object_path = PathBuf::from(OsString::from_vec(b"/tmp/jni-\xFE.o".to_vec()));
        let library_path = PathBuf::from(OsString::from_vec(b"/tmp/lib-\xFD.a".to_vec()));
        let export_script_path =
            PathBuf::from(OsString::from_vec(b"/tmp/exports-\xFC.map".to_vec()));
        let args = android_shared_link_args(
            &dest_path,
            &object_path,
            &library_path,
            &export_script_path,
            &[],
        );

        assert_eq!(
            args[2].as_os_str().as_bytes(),
            dest_path.as_os_str().as_bytes()
        );
        assert_eq!(
            args[3].as_os_str().as_bytes(),
            object_path.as_os_str().as_bytes()
        );
        assert_eq!(
            args[5].as_os_str().as_bytes(),
            library_path.as_os_str().as_bytes()
        );
        assert_eq!(
            args[10].as_os_str().as_bytes(),
            export_script_path.as_os_str().as_bytes()
        );
    }
}
