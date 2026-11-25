//! The staging gate: restaging the transcribe libraries must not
//! invalidate a mapping a live process already holds.
//!
//! A staged library that is rewritten in place keeps its inode, so the
//! file's mtime moves under a process that mapped the old content, and
//! macOS rejects the process's next executable page fault
//! (`cs_invalid_page`, `cs_mtime != mtime`) with SIGKILL. A stage that
//! writes a new file and renames it over the target never touches the
//! inode a running binary holds. The gate exercises that property
//! against the same `stage()` a build runs: a child process maps the
//! staged library, the test restages a second build, the child must
//! survive its next executable fault, and a NEW process must then load
//! the restaged build and answer with its own version - survival alone
//! would also pass an empty, corrupt or foreign replacement.
//!
//! The child is a small C program the test builds with clang, because
//! the workspace forbids `unsafe`, and mapping and calling into a dylib
//! is inherently unsafe. It is the only way to observe the kernel's
//! page rejection from a test.
//!
//! macOS-only: the rejection is dyld's code-signing check. On a host
//! whose PATH lacks clang or codesign the test skips with the reason
//! printed, rather than passing silently.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::staging::stage;

/// Two builds of the same library with different answers, so a mapping
/// that survives can say which file it now reads.
const PROBE_V1: &str = "int probe(void) { return 45; }\n";
const PROBE_V2: &str = "int probe(void) { return 47; }\n";

/// The child: load the staged library, report its first answer, then
/// wait to be released. On release it maps the library's page afresh
/// from the path and calls it. An in-place rewrite makes that fault
/// fatal; an atomic rename either hands back the new content or is
/// refused because the path is a different file now - both are the
/// survival this gate asserts.
const HELPER: &str = r#"
#include <dlfcn.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static unsigned long text_fileoff(void *address, unsigned long *slide_out) {
    Dl_info info;
    if (dladdr(address, &info) == 0) {
        return 0;
    }
    *slide_out = (unsigned long)info.dli_fbase;
    uint32_t count = _dyld_image_count();
    for (uint32_t i = 0; i < count; i++) {
        struct mach_header_64 *header = (struct mach_header_64 *)_dyld_get_image_header(i);
        if ((void *)header != info.dli_fbase) {
            continue;
        }
        unsigned char *cursor = (unsigned char *)header + sizeof(*header);
        for (uint32_t slot = 0; slot < header->ncmds; slot++) {
            struct load_command *command = (struct load_command *)cursor;
            if (command->cmd == LC_SEGMENT_64) {
                struct segment_command_64 *segment = (struct segment_command_64 *)command;
                if (strcmp(segment->segname, "__TEXT") == 0) {
                    return (unsigned long)segment->fileoff;
                }
            }
            cursor += command->cmdsize;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    void *handle = dlopen(argv[1], RTLD_LAZY | RTLD_LOCAL);
    if (handle == NULL) {
        printf("dlopen failed: %s\n", dlerror());
        return 2;
    }
    void *symbol = dlsym(handle, "probe");
    if (symbol == NULL) {
        printf("dlsym failed: %s\n", dlerror());
        return 3;
    }
    printf("first %ld\n", ((long (*)(void))symbol)());
    fflush(stdout);
    int waited = 0;
    while (access(argv[2], F_OK) != 0) {
        if (++waited > 6000) {
            printf("release never came\n");
            return 6;
        }
        usleep(10000);
    }
    long page = sysconf(_SC_PAGESIZE);
    unsigned long slide = 0;
    unsigned long fileoff = text_fileoff(symbol, &slide);
    unsigned long target = (unsigned long)symbol & ~(unsigned long)(page - 1);
    unsigned long offset = (target - slide) + fileoff;
    unsigned long in_page = (unsigned long)symbol & (unsigned long)(page - 1);
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        printf("open failed\n");
        return 4;
    }
    unsigned char *mapping = mmap(NULL, (size_t)page, PROT_READ | PROT_EXEC,
                                  MAP_PRIVATE, fd, (off_t)offset);
    if (mapping == MAP_FAILED) {
        printf("refused\n");
        return 0;
    }
    printf("after %ld\n", ((long (*)(void))(mapping + in_page))());
    fflush(stdout);
    return 0;
}
"#;

fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| {
        std::fs::metadata(candidate)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

fn run_tool(tool: &Path, args: &[&Path]) -> std::io::Result<()> {
    let output = Command::new(tool).args(args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "{} failed with {}: {}",
            tool.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn build_dylib(clang: &Path, codesign: &Path, source: &Path, out: &Path) -> std::io::Result<()> {
    run_tool(clang, &[Path::new("-dynamiclib"), Path::new("-o"), out, source])?;
    run_tool(codesign, &[Path::new("--force"), Path::new("--sign"), Path::new("-"), out])
}

#[test]
fn restaging_the_libraries_does_not_invalidate_a_live_mapping()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(clang) = find_tool("clang") else {
        println!("skipping the staging gate: clang is not on PATH, so its fixture cannot be built");
        return Ok(());
    };
    let Some(codesign) = find_tool("codesign") else {
        println!(
            "skipping the staging gate: codesign is not on PATH, so its fixture cannot be signed"
        );
        return Ok(());
    };

    let root = tempfile::tempdir()?;
    let earlier_source = root.path().join("earlier");
    let later_source = root.path().join("later");
    let dest = root.path().join("staged");
    for dir in [&earlier_source, &later_source, &dest] {
        std::fs::create_dir_all(dir)?;
    }
    let earlier_file = earlier_source.join("probe.c");
    let later_file = later_source.join("probe.c");
    std::fs::write(&earlier_file, PROBE_V1)?;
    std::fs::write(&later_file, PROBE_V2)?;
    let earlier_dylib = earlier_source.join("libprobe.dylib");
    let later_dylib = later_source.join("libprobe.dylib");
    build_dylib(&clang, &codesign, &earlier_file, &earlier_dylib)?;
    build_dylib(&clang, &codesign, &later_file, &later_dylib)?;

    let helper_source = root.path().join("helper.c");
    let helper = root.path().join("helper");
    std::fs::write(&helper_source, HELPER)?;
    run_tool(&clang, &[Path::new("-O1"), Path::new("-o"), &helper, &helper_source])?;

    stage(&earlier_source, &dest)?;
    let staged = dest.join("libprobe.dylib");
    let earlier_inode = std::fs::metadata(&staged)?.ino();
    let release = root.path().join("release");
    let mut child =
        Command::new(&helper).arg(&staged).arg(&release).stdout(Stdio::piped()).spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the helper's stdout must be piped"))?;
    let mut lines = BufReader::new(stdout).lines();
    let first = lines
        .next()
        .ok_or_else(|| std::io::Error::other("the helper must report its first call"))??;
    assert_eq!(first.trim(), "first 45", "the staged library must load at version one");

    stage(&later_source, &dest)?;
    std::fs::write(&release, b"go")?;

    let rest: Vec<String> = lines.collect::<std::io::Result<Vec<String>>>()?;
    let status = child.wait()?;
    assert!(
        status.success(),
        "the helper died (status {status:?}, signal {:?}) after the libraries were restaged: a \
         live mapping was invalidated, so the stage rewrote a file in place instead of renaming \
         a new one over it. The helper said: {rest:?}",
        status.signal()
    );
    let later_inode = std::fs::metadata(&staged)?.ino();
    assert_ne!(
        earlier_inode, later_inode,
        "the restage must install a new inode: rewriting the mapped file in place is what macOS \
         rejects, and a refused remap must not be read as survival when the file never changed"
    );
    let observed = rest.last().map_or("nothing after the release", String::as_str);

    // **The first child's survival is only half the proof.** Its late remap
    // is refused on this machine because the path is a different file now,
    // and an empty, corrupt or foreign-architecture replacement would leave
    // it just as alive. The restaged build must load in a process that never
    // saw the earlier one - dyld deduplicates by path inside a process - so
    // run the helper a second time and require the later build's answer.
    let mut verifier =
        Command::new(&helper).arg(&staged).arg(&release).stdout(Stdio::piped()).spawn()?;
    let stdout = verifier
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the verifier's stdout must be piped"))?;
    let mut verifier_lines = BufReader::new(stdout).lines();
    let loaded = verifier_lines
        .next()
        .ok_or_else(|| std::io::Error::other("the verifier must report its first call"))??;
    assert_eq!(
        loaded.trim(),
        "first 47",
        "a new process must load the restaged build; an empty, corrupt or foreign file at a new \
         inode must not pass"
    );
    let verifier_rest: Vec<String> = verifier_lines.collect::<std::io::Result<Vec<String>>>()?;
    let verifier_status = verifier.wait()?;
    assert!(
        verifier_status.success(),
        "the verifier must exit cleanly: {verifier_status:?}; it said {verifier_rest:?}"
    );
    println!(
        "the restage left the live mapping valid ({observed}) and a new process loads the later \
         build"
    );
    Ok(())
}
