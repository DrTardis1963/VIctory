//! `victory triage <bundle>`: what a crash bundle says, in one screen.
//!
//! A run writes a bundle (`crash-<unix time>-<pid>.txt`) beside
//! `crashes.log` when the title crashes or traps. This prints its summary:
//! the exception, where it happened, the stack, the last HLE calls and the
//! unimplemented imports. With `--decompile` it also runs headless Ghidra on
//! the faulting function of the mapped image the bundle's module map names
//! (a file outside the repository), and writes the C beside the bundle.
//! Ghidra is optional: without it, the summary is all there is.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use victory_loader::bundle::{Bundle, split_location};

pub fn run(path: &Path, decompile: bool) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    anyhow::ensure!(
        Bundle::is_bundle(&text),
        "{} is not a crash bundle (it does not start with {:?})",
        path.display(),
        victory_loader::bundle::MAGIC
    );
    let bundle = Bundle::parse(&text);
    print!("{}", summary(&bundle));
    if decompile {
        println!();
        println!("{}", decompile_fault(&bundle, path, Ghidra::find()));
    }
    Ok(())
}

/// Lines shown of each long section.
const SHOWN: usize = 16;

fn summary(bundle: &Bundle) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| out.push_str(&format!("{label:<14} {value}\n"));
    let report = bundle.section("report");
    line("crash", report.first().map_or("(no report)", |l| l.trim()));
    for key in ["process", "unix time", "fault", "fault function"] {
        if let Some(value) = bundle.field(key) {
            line(key, value);
        }
    }
    if let Some(module) = fault_location(bundle).and_then(|(m, _)| bundle.module_named(m)) {
        let present = if Path::new(&module.path).is_file() {
            ""
        } else {
            " (no longer there)"
        };
        line("image", &format!("{}{present}", module.path));
    }
    let mut section = |title: &str, lines: Vec<&str>, from_end: bool| {
        out.push_str(&format!("\n{title} ({}):\n", lines.len()));
        let skip = if from_end {
            lines.len().saturating_sub(SHOWN)
        } else {
            0
        };
        for l in lines.iter().skip(skip).take(SHOWN) {
            out.push_str(&format!("  {}\n", l.trim()));
        }
        if lines.len() > SHOWN {
            let which = if from_end { "earlier" } else { "more" };
            out.push_str(&format!("  ... {} {which}\n", lines.len() - SHOWN));
        }
    };
    let trimmed = |name: &str| -> Vec<&str> {
        bundle
            .section(name)
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect()
    };
    let stack: Vec<&str> = report
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with('#'))
        .collect();
    let registers: Vec<&str> = report
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("rax ") || l.starts_with("r8 "))
        .collect();
    section("registers", registers, false);
    section("stack", stack, false);
    section("last hle calls", trimmed("hle calls"), true);
    section(
        "unimplemented imports called",
        trimmed("unimplemented"),
        false,
    );
    section("title output", trimmed("title output"), true);
    out
}

/// The function the fault is in (its start, when the unwind data named it),
/// else the faulting address, as module and rva.
fn fault_location(bundle: &Bundle) -> Option<(&str, u64)> {
    bundle
        .field("fault function")
        .or(bundle.field("fault"))
        .and_then(split_location)
}

/// Where headless Ghidra and its JDK are.
#[derive(Debug)]
struct Ghidra {
    headless: PathBuf,
    jdk: PathBuf,
}

impl Ghidra {
    /// `VICTORY_GHIDRA` (the install folder), by default `tools\ghidra` in
    /// the user's profile; the first of `VICTORY_JDK`, `tools\jdk21` in the
    /// profile and `JAVA_HOME` that this Ghidra can run on (a stale
    /// `JAVA_HOME` is common).
    fn find() -> Result<Ghidra, String> {
        let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
        let tools = |name: &str| profile.as_ref().map(|p| p.join("tools").join(name));
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let install = var("VICTORY_GHIDRA")
            .or_else(|| tools("ghidra"))
            .ok_or("no user profile folder")?;
        let jdks: Vec<PathBuf> = [var("VICTORY_JDK"), tools("jdk21"), var("JAVA_HOME")]
            .into_iter()
            .flatten()
            .collect();
        let mut why = "no JDK named (set VICTORY_JDK to a JDK folder)".to_owned();
        for jdk in &jdks {
            match Ghidra::at(&install, jdk) {
                Ok(ghidra) => return Ok(ghidra),
                Err(e) => why = e,
            }
        }
        // The first one's reason names the JDK most likely meant.
        jdks.first()
            .map_or(Err(why), |jdk| Ghidra::at(&install, jdk))
    }

    fn at(install: &Path, jdk: &Path) -> Result<Ghidra, String> {
        let headless = install.join("support").join("analyzeHeadless.bat");
        if !headless.is_file() {
            return Err(format!(
                "no Ghidra at {} (set VICTORY_GHIDRA to its folder)",
                install.display()
            ));
        }
        if !jdk.join("bin").join("java.exe").is_file() {
            return Err(format!(
                "no JDK at {} (set VICTORY_JDK to a JDK folder)",
                jdk.display()
            ));
        }
        // Ghidra asks on the console for another JDK when this one is too
        // old; say so here instead.
        let properties = install.join("Ghidra").join("application.properties");
        let needed = property(&properties, "application.java.min=");
        let has = property(&jdk.join("release"), "JAVA_VERSION=");
        let major = |v: &str| v.trim_matches('"').split('.').next()?.parse::<u32>().ok();
        if let (Some(needed), Some(has)) = (needed.as_deref().and_then(major), has)
            && major(&has).is_some_and(|has| has < needed)
        {
            return Err(format!(
                "this Ghidra needs JDK {needed} or newer, and the JDK at {} is {} \
                 (set VICTORY_JDK to a newer one)",
                jdk.display(),
                has.trim_matches('"')
            ));
        }
        Ok(Ghidra {
            headless,
            jdk: jdk.to_owned(),
        })
    }
}

/// The value after `key` on its line of the text file `path`.
fn property(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix(key))
        .map(|v| v.trim().to_owned())
}

/// The Ghidra script: decompiles the function at an rva (its first
/// argument) into a file (its second). The import has no auto-analysis (a
/// title's image takes hours), so the script disassembles from that
/// address and makes the function itself.
const SCRIPT_NAME: &str = "VictoryTriageDecompile.java";
const SCRIPT: &str = r#"// Decompiles the function at an rva, for `victory triage --decompile`.
// @category VIctory
import ghidra.app.decompiler.DecompInterface;
import ghidra.app.decompiler.DecompileResults;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.Address;
import ghidra.program.model.listing.Function;
import java.nio.file.Files;
import java.nio.file.Path;

public class VictoryTriageDecompile extends GhidraScript {
    @Override
    public void run() throws Exception {
        String[] args = getScriptArgs();
        Address at = currentProgram.getImageBase().add(Long.decode(args[0]));
        disassemble(at);
        Function f = getFunctionContaining(at);
        if (f == null) {
            f = createFunction(at, null);
        }
        String text;
        if (f == null) {
            text = "// no function could be made at " + at + "\n";
        } else {
            DecompInterface decompiler = new DecompInterface();
            decompiler.openProgram(currentProgram);
            DecompileResults result = decompiler.decompileFunction(f, 300, monitor);
            text = result.decompileCompleted()
                ? "// " + f.getName() + " at " + f.getEntryPoint() + "\n"
                    + result.getDecompiledFunction().getC()
                : "// decompiling failed: " + result.getErrorMessage() + "\n";
            decompiler.dispose();
        }
        Files.writeString(Path.of(args[1]), text);
    }
}
"#;

/// Decompiles the faulting function; returns what to print. Every problem
/// (no Ghidra, no image, Ghidra failing) is reported, not raised: the
/// summary above stays useful on its own.
fn decompile_fault(bundle: &Bundle, bundle_path: &Path, ghidra: Result<Ghidra, String>) -> String {
    let Some((module, rva)) = fault_location(bundle) else {
        return "decompile: skipped, the bundle names no fault inside a module".into();
    };
    let Some(image) = bundle.module_named(module) else {
        return format!("decompile: skipped, {module} is not in the bundle's module map");
    };
    let binary = PathBuf::from(&image.path);
    if !binary.is_file() {
        return format!(
            "decompile: skipped, {} is no longer there",
            binary.display()
        );
    }
    let ghidra = match ghidra {
        Ok(ghidra) => ghidra,
        Err(why) => return format!("decompile: skipped, {why}"),
    };
    match run_ghidra(&ghidra, &binary, rva, bundle_path) {
        Ok((out, text)) => format!(
            "decompiled {module}+{rva:#x} (written to {}):\n\n{text}",
            out.display()
        ),
        Err(error) => format!("decompile: failed: {error:#}"),
    }
}

fn run_ghidra(
    ghidra: &Ghidra,
    binary: &Path,
    rva: u64,
    bundle: &Path,
) -> Result<(PathBuf, String)> {
    let devtools = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is not set")?
        .join("victory-devtools");
    let projects = devtools.join("ghidra_projects");
    let scripts = devtools.join("ghidra_scripts");
    let out = bundle.with_extension("decompiled.c");
    for path in [&projects, &scripts, &out] {
        victory_core::ensure_outside_repo(path)?;
    }
    std::fs::create_dir_all(&projects)?;
    std::fs::create_dir_all(&scripts)?;
    std::fs::write(scripts.join(SCRIPT_NAME), SCRIPT)?;
    let _ = std::fs::remove_file(&out);

    // One project per image (name and size), so a later triage of the same
    // build skips the import.
    let name = binary
        .file_name()
        .context("the image path has no file name")?
        .to_string_lossy()
        .into_owned();
    let size = std::fs::metadata(binary)?.len();
    let stem: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let project = format!("triage-{stem}-{size:x}");
    let mut command = Command::new(&ghidra.headless);
    command.arg(&projects).arg(&project);
    if projects.join(format!("{project}.gpr")).is_file() {
        command.args(["-process", &name]);
    } else {
        command.arg("-import").arg(binary);
    }
    command
        .args(["-noanalysis", "-scriptPath"])
        .arg(&scripts)
        .args(["-postScript", SCRIPT_NAME, &format!("{rva:#x}")])
        .arg(&out)
        .env("JAVA_HOME", &ghidra.jdk)
        // It must never wait for an answer on the console.
        .stdin(std::process::Stdio::null());
    println!(
        "running Ghidra on {} (the first time imports it, a minute or more)",
        binary.display()
    );
    let ran = command.output().context("starting analyzeHeadless")?;
    match std::fs::read_to_string(&out) {
        Ok(text) => Ok((out, text)),
        Err(_) => {
            let log = String::from_utf8_lossy(&ran.stdout);
            let tail: Vec<&str> = log.lines().rev().take(15).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            anyhow::bail!(
                "Ghidra wrote nothing ({}); its last lines:\n{}",
                ran.status,
                tail.join("\n")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(image: &str) -> Bundle {
        Bundle::parse(&format!(
            "{}\nprocess: 42\nfault: title.exe+0x1234\nfault function: title.exe+0x1200\n\n\
             == report ==\n\
             VIctory: unhandled exception 0xc0000005 at title.exe+0x1234 (read 0x10: nothing mapped there)\n\
             \x20 rax 0x1 rbx 0x2\n\
             \x20 r8 0x3 r9 0x4\n\
             \x20 #0  title.exe+0x1234\n\
             \x20 #1  title.exe+0x99\n\
             == modules ==\n\
             \x20 0x140000000 0x20000 {image}\n\
             == hle calls ==\n\
             \x20 #7 t=10 thread=3 xmem.dll!XMemAlloc args=[0x10, 0x0, 0x0, 0x0] ret=0x5000 from title.exe+0x50\n\
             == unimplemented ==\n\
             \x20 unimplemented: a.dll!F x2 (first from title.exe+0x10)\n",
            victory_loader::bundle::MAGIC
        ))
    }

    #[test]
    fn the_summary_names_the_crash_its_stack_and_calls() {
        let text = summary(&bundle(r"Z:\nowhere\title.exe"));
        assert!(
            text.contains("crash          VIctory: unhandled exception 0xc0000005"),
            "{text}"
        );
        assert!(text.contains("fault function title.exe+0x1200"), "{text}");
        assert!(
            text.contains(r"image          Z:\nowhere\title.exe (no longer there)"),
            "{text}"
        );
        assert!(
            text.contains("stack (2):\n  #0  title.exe+0x1234\n  #1  title.exe+0x99"),
            "{text}"
        );
        assert!(text.contains("registers (2):"), "{text}");
        assert!(text.contains("xmem.dll!XMemAlloc"), "{text}");
        assert!(text.contains("a.dll!F x2"), "{text}");
    }

    #[test]
    fn decompiling_without_ghidra_or_the_image_is_skipped_softly() {
        let gone = bundle(r"Z:\nowhere\title.exe");
        let said = decompile_fault(&gone, Path::new("b.txt"), Err("no Ghidra".into()));
        assert!(said.contains("no longer there"), "{said}");
        // An image that exists (this test's own executable), but no Ghidra.
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        let here = Bundle::parse(&format!(
            "{}\nfault: {name}+0x10\n\n== modules ==\n  0x1 0x1000 {}\n",
            victory_loader::bundle::MAGIC,
            exe.display()
        ));
        let missing = Ghidra::at(Path::new(r"Z:\no\ghidra"), Path::new(r"Z:\no\jdk"));
        let said = decompile_fault(&here, Path::new("b.txt"), missing);
        assert!(
            said.starts_with("decompile: skipped, no Ghidra at"),
            "{said}"
        );
        // A Ghidra that needs a newer JDK than the one named.
        let dir = std::env::temp_dir().join(format!("victory-triage-{}", std::process::id()));
        let (install, jdk) = (dir.join("ghidra"), dir.join("jdk"));
        std::fs::create_dir_all(install.join("support")).unwrap();
        std::fs::create_dir_all(install.join("Ghidra")).unwrap();
        std::fs::create_dir_all(jdk.join("bin")).unwrap();
        std::fs::write(install.join("support/analyzeHeadless.bat"), "").unwrap();
        let min = "application.java.min=25\n";
        std::fs::write(install.join("Ghidra/application.properties"), min).unwrap();
        std::fs::write(jdk.join("bin/java.exe"), "").unwrap();
        std::fs::write(jdk.join("release"), "JAVA_VERSION=\"21.0.13\"\n").unwrap();
        let old = Ghidra::at(&install, &jdk).unwrap_err();
        assert!(
            old.contains("needs JDK 25 or newer") && old.contains("is 21.0.13"),
            "{old}"
        );
        std::fs::write(jdk.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();
        assert!(Ghidra::at(&install, &jdk).is_ok());
        let _ = std::fs::remove_dir_all(&dir);

        let none = Bundle::parse(victory_loader::bundle::MAGIC);
        let said = decompile_fault(&none, Path::new("b.txt"), Err("x".into()));
        assert!(said.contains("names no fault"), "{said}");
    }
}
