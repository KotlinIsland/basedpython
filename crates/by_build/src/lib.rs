//! the native build driver
//!
//! takes verified BIR, emits C, writes the runtime header beside it, and invokes
//! the platform C compiler to produce a loadable extension module.

pub mod toolchain;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use by_ir::function::ModuleIr;
use by_ir::verify::verify_module;

pub use toolchain::Toolchain;

pub mod annotate;

/// what a build produced
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// the generated C, kept for `--annotate` and for debugging
    pub source: PathBuf,
    /// the loadable extension
    pub extension: PathBuf,
    /// the `--annotate` report, when one was asked for
    pub annotation: Option<PathBuf>,
}

/// lower `.by` source and compile it to a native extension in `out_dir`
///
/// the whole pipeline in one call: parse and check, lower to BIR, verify, emit
/// C, and invoke the platform compiler
pub fn build_source(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    toolchain: &Toolchain,
    out_dir: &Path,
    options: &Options,
) -> Result<Built> {
    let module = lower(source, module_name, options, toolchain.version)?;
    let mut artifact = build_module(&module, toolchain, out_dir)?;
    artifact.annotation = write_annotation(&module, out_dir, options)?;
    Ok(Built {
        artifact,
        declined: module.declined,
    })
}

/// compile a module the caller lowered itself
///
/// `by compile` uses this: a whole-project build lowers every file against one
/// project database, so cross-module types resolve. lowering per file instead
/// makes an imported class gradual, which is sound — it degrades to the object
/// protocol — but reports `--no-any` failures that are pure noise
pub fn build_lowered(
    module: ModuleIr,
    source: &str,
    toolchain: &Toolchain,
    out_dir: &Path,
    options: &Options,
) -> Result<Built> {
    let module = finish(module, source, options, toolchain.version)?;
    let mut artifact = build_module(&module, toolchain, out_dir)?;
    artifact.annotation = write_annotation(&module, out_dir, options)?;
    Ok(Built {
        artifact,
        declined: module.declined,
    })
}

/// as [`build_lowered`], but writing only the generated C
pub fn emit_lowered(
    module: ModuleIr,
    source: &str,
    out_dir: &Path,
    options: &Options,
) -> Result<Built> {
    let module = finish(module, source, options, None)?;
    emit_verified(&module, out_dir, options)
}

/// lower `.by` source and write the generated C, without invoking a compiler
///
/// the extension path in the returned [`Artifact`] is where the extension *would*
/// be written, so `--emit-c-only` and a real build report the same layout
pub fn emit_source(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    out_dir: &Path,
    options: &Options,
) -> Result<Built> {
    let module = lower(source, module_name, options, None)?;
    emit_verified(&module, out_dir, options)
}

/// verify a lowered module and write its C
fn emit_verified(module: &ModuleIr, out_dir: &Path, options: &Options) -> Result<Built> {
    if let Err(errors) = verify_module(module) {
        let detail = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!("the generated IR is not well-formed:\n  {detail}");
    }
    fs::create_dir_all(out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;
    fs::write(out_dir.join(by_rt::BY_H_NAME), by_rt::BY_H)?;

    let source_path = out_dir.join(module.name.relative_path(".c"));
    create_parent(&source_path)?;
    fs::write(&source_path, by_codegen_c::emit_module(module))
        .with_context(|| format!("could not write {}", source_path.display()))?;

    Ok(Built {
        artifact: Artifact {
            source: source_path,
            extension: out_dir.join(module.name.relative_path(".so")),
            annotation: write_annotation(module, out_dir, options)?,
        },
        declined: module.declined.clone(),
    })
}

/// make the directory `path` is to be written into
///
/// an artefact sits at its module's own place in the output tree, so a package
/// member's directory may not exist yet
fn create_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))
}

/// what a build is allowed to leave interpreted
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// reject a function declined because a type was gradual, instead of
    /// quietly leaving it interpreted.
    ///
    /// this buys no speed on its own — it is a *predictability* contract. a
    /// gradual type is the reason most functions decline, and a decline is
    /// invisible unless you look, so a module that means to be fully compiled
    /// should be able to say so and be held to it
    pub no_any: bool,
    /// reject *any* decline, whatever the reason.
    ///
    /// stricter than [`Self::no_any`], and a different question: `no_any` asks
    /// "is this module fully typed", this asks "does this module compile
    /// entirely", which also fails on a type the compiler simply does not
    /// represent yet
    pub require_native: bool,
    /// write an [`annotate`] report next to the generated C.
    ///
    /// a decline is invisible unless you look, and the printed count says how many
    /// without saying which
    pub annotate: bool,
    /// which language the source is written in
    ///
    /// it decides where a declined function's interpreted definition comes from.
    /// a `.by` source has to be transpiled before anything can run from it; a `.py`
    /// source *is* the thing that runs, and transpiling one anyway would be a round
    /// trip through a different program — the transpiler inserts soundness checks
    /// and sentinels of its own — so it is used verbatim
    pub language: by_irbuild::Language,
    /// the transpiler configuration for the interpreted fallback, when there is
    /// one to transpile
    ///
    /// `None` means the default. it matters because a declined function *runs*
    /// from this source, so a build that means to insert extra soundness checks
    /// has to insert them here too or the two halves of the module disagree
    pub fallback: Option<by_transforms::Config>,
}

/// write the `--annotate` report, when one was asked for
fn write_annotation(
    module: &ModuleIr,
    out_dir: &Path,
    options: &Options,
) -> Result<Option<PathBuf>> {
    if !options.annotate {
        return Ok(None);
    }
    let path = out_dir.join(module.name.relative_path(".annotated"));
    create_parent(&path)?;
    fs::write(&path, annotate::report(module))
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(Some(path))
}

fn render_declines<'a>(declines: impl Iterator<Item = &'a by_ir::function::Declined>) -> String {
    declines
        .map(|declined| format!("  {}: {}", declined.name, declined.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

/// lower `.by` source, attaching the transpiled python that supplies the
/// module's interpreted definitions
fn lower(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    options: &Options,
    version: Option<(u8, u8)>,
) -> Result<by_ir::function::ModuleIr> {
    finish(
        by_irbuild::module_from_source(source, module_name, options.language),
        source,
        options,
        version,
    )
}

/// apply the gates, optimize, and attach the interpreted fallback
///
/// split out from [`lower`] because a whole-project build lowers against a real
/// project database — one the caller owns — and only this half is shared
fn finish(
    mut module: by_ir::function::ModuleIr,
    source: &str,
    options: &Options,
    version: Option<(u8, u8)>,
) -> Result<by_ir::function::ModuleIr> {
    // the generated C points back at the `.by` it came from, so a compiler warning
    // or a debugger lands on source somebody wrote. a caller that knows the real
    // path sets this itself — the bare module name is the fallback
    if module.lines.is_none() {
        let path = module
            .name
            .relative_path(&format!(".{}", options.language.extension()));
        module.lines = Some(by_ir::function::LineTable::new(
            path.display().to_string(),
            source,
        ));
    }

    if options.require_native && !module.declined.is_empty() {
        bail!(
            "`require-native` is on and {} function(s) were left interpreted:\n{}",
            module.declined.len(),
            render_declines(module.declined.iter())
        );
    }
    if options.no_any && !module.gradual.is_empty() {
        let places = module
            .gradual
            .iter()
            .map(|use_| format!("  {}: `{}` is gradual", use_.function, use_.place))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "`no-any` is on and {} place(s) in this module have a gradual type:\n{places}",
            module.gradual.len()
        );
    }

    by_opt::optimize(&mut module).map_err(|(pass, errors)| {
        let detail = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::anyhow!("the `{pass}` pass produced ill-formed IR:\n  {detail}")
    })?;

    // the default config on purpose, except for the target version: the
    // lazy-import pass is what binds `JustFloat = float` locally instead of
    // emitting a `from ty_extensions import ...` that has no module behind it.
    //
    // the version has to be the *interpreter's*, because this python runs inside
    // the extension at import time — emitting syntax the interpreter cannot parse
    // makes the whole module fail to load, taking every function with it
    // a `.py` source is already what runs: it is its own fallback
    let twin = if options.language == by_irbuild::Language::Python {
        source.to_string()
    } else {
        let mut config = options.fallback.clone().unwrap_or_default();
        if let Some((major, minor)) = version
            && let Ok(parsed) = format!("{major}.{minor}").parse()
        {
            config.min_version = parsed;
        }
        by_transforms::transpile(source, &config).map_err(|error| {
            anyhow::anyhow!("could not transpile for the interpreted fallback: {error}")
        })?
    };
    // a decorator module init applies to the native definition would otherwise run here
    // too, over the twin's — once for each definition rather than once for the name
    let twin = by_irbuild::without_init_decorators(&twin, &module)
        .map_err(|error| anyhow::anyhow!("could not prepare the interpreted fallback: {error}"))?;
    module.fallback_source = Some(twin);
    Ok(module)
}

/// a build, plus what it could not lower natively
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub artifact: Artifact,
    /// each function left to the interpreted definition
    pub declined: Vec<by_ir::function::Declined>,
}

/// compile a module to a native extension in `out_dir`
///
/// the module is verified first, unconditionally: codegen is only correct for
/// verified BIR, so skipping the check would trade a clear error for a
/// miscompile
pub fn build_module(module: &ModuleIr, toolchain: &Toolchain, out_dir: &Path) -> Result<Artifact> {
    if let Err(errors) = verify_module(module) {
        let detail = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!("the generated IR is not well-formed:\n  {detail}");
    }

    fs::create_dir_all(out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;

    // the header stays at the root of the output tree, and the root is what is put
    // on the include path — so one copy serves every artefact however deep its
    // package goes
    let header = out_dir.join(by_rt::BY_H_NAME);
    let header_changed = write_if_changed(&header, by_rt::BY_H.as_bytes())?;

    let source = out_dir.join(module.name.relative_path(".c"));
    create_parent(&source)?;
    let source_changed = write_if_changed(&source, by_codegen_c::emit_module(module).as_bytes())?;

    let extension = out_dir.join(toolchain.extension_path(&module.name));
    // the C compiler is by far the slowest step, and the emitted C is a faithful
    // function of the optimized BIR — so identical C means an identical compile.
    // keying on the C rather than on the `.by` is what makes a comment-only edit,
    // or a renamed local, free
    if header_changed || source_changed || !is_newer(&extension, &source) {
        compile(toolchain, &source, &extension, out_dir)?;
    }

    Ok(Artifact {
        source,
        extension,
        annotation: None,
    })
}

/// write `contents` only if the file does not already hold exactly that
///
/// leaving an unchanged file alone is the whole point: its mtime stays put, so the
/// artifact built from it still looks up to date
fn write_if_changed(path: &Path, contents: &[u8]) -> Result<bool> {
    if fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok(false);
    }
    fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))?;
    Ok(true)
}

/// whether `artifact` exists and is at least as new as `source`
fn is_newer(artifact: &Path, source: &Path) -> bool {
    let modified = |path: &Path| fs::metadata(path).and_then(|meta| meta.modified()).ok();
    match (modified(artifact), modified(source)) {
        (Some(artifact), Some(source)) => artifact >= source,
        // no artifact, or a filesystem that will not say — build it
        _ => false,
    }
}

/// the compiler invocation, split out so the argument list is testable without
/// running a compiler
pub fn compile_command(
    toolchain: &Toolchain,
    source: &Path,
    output: &Path,
    include_dir: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = toolchain.cc.clone();
    args.push("-O2".to_string());
    args.extend(toolchain.compile_flags.iter().cloned());
    // the generated C is machine-written; warnings about it are noise the user
    // cannot act on
    args.push("-w".to_string());
    args.push(format!("-I{}", include_dir.display()));
    for dir in &toolchain.include_dirs {
        args.push(format!("-I{}", dir.display()));
    }
    for dir in &toolchain.library_dirs {
        args.push(format!("-L{}", dir.display()));
    }
    args.extend(toolchain.link_flags.iter().cloned());
    args.push(source.display().to_string());
    // after the translation unit that needs them: a linker resolving left to right has
    // nothing to take from a library it has already passed
    for library in &toolchain.link_libraries {
        args.push(library.display().to_string());
    }
    args.push("-o".to_string());
    args.push(output.display().to_string());
    args
}

fn compile(toolchain: &Toolchain, source: &Path, output: &Path, include_dir: &Path) -> Result<()> {
    let args = compile_command(toolchain, source, output, include_dir);
    let (program, rest) = args
        .split_first()
        .context("the toolchain reported an empty compiler command")?;

    let result = Command::new(program)
        .args(rest)
        .output()
        .with_context(|| format!("could not run the C compiler `{program}`"))?;

    if !result.status.success() {
        bail!(
            "the C compiler rejected the generated code\n  command: {}\n  {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{Terminator, Value};
    use by_ir::rtype::RType;

    fn toolchain() -> Toolchain {
        Toolchain::from_probe(
            "python3",
            r#"{"cc": ["cc"], "include": ["/inc"], "libdir": ["/lib"], "ext_suffix": ".so", "platform": "linux"}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_command_carries_both_include_paths_and_the_output() {
        let args = compile_command(
            &toolchain(),
            Path::new("/tmp/app.c"),
            Path::new("/tmp/app.so"),
            Path::new("/tmp"),
        );
        assert_eq!(args[0], "cc");
        assert!(
            args.contains(&"-I/tmp".to_string()),
            "the runtime header dir"
        );
        assert!(args.contains(&"-I/inc".to_string()), "the cpython headers");
        assert!(args.contains(&"-L/lib".to_string()));
        assert!(args.contains(&"-shared".to_string()));
        assert!(args.contains(&"-fPIC".to_string()));
        assert_eq!(args[args.len() - 2], "-o");
        assert_eq!(args[args.len() - 1], "/tmp/app.so");
    }

    #[test]
    fn a_windows_link_names_the_import_library_after_the_source() {
        // an unlinked `Py*` is a link error on windows rather than a symbol the loader
        // fills in, and a linker resolving left to right takes nothing from a library
        // it has already passed
        let toolchain = Toolchain::from_probe(
            "python3",
            r#"{"cc": ["cc"], "include": ["C:\\py\\Include"], "libdir": [], "libs": ["C:\\py\\libs\\python312.lib"], "ext_suffix": ".pyd", "platform": "win32"}"#,
        )
        .unwrap();
        let args = compile_command(
            &toolchain,
            Path::new("app.c"),
            Path::new("app.pyd"),
            Path::new("build"),
        );
        let source = args.iter().position(|arg| arg == "app.c").unwrap();
        let library = args
            .iter()
            .position(|arg| arg == r"C:\py\libs\python312.lib")
            .expect("the import library is on the command");
        assert!(source < library, "{args:?}");
        assert!(!args.contains(&"-fPIC".to_string()), "{args:?}");
    }

    #[test]
    fn ill_formed_ir_is_refused_before_any_file_is_written() {
        let mut builder = FunctionBuilder::new("bad", RType::INT);
        // returning a float from an int function
        builder.terminate(Terminator::Return(Value::Float(1.0)));
        let module = ModuleIr {
            name: by_ir::ModuleName::new("bad"),
            functions: vec![builder.finish()],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
        };
        let dir = std::env::temp_dir().join("by_build_refuses_test");
        let _ = fs::remove_dir_all(&dir);
        let error = build_module(&module, &toolchain(), &dir).unwrap_err();
        assert!(error.to_string().contains("not well-formed"));
        assert!(!dir.exists(), "nothing is written for ill-formed IR");
    }
}
