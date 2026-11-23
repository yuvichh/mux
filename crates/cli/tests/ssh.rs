//! Drives the real binary against a machine reached by SSH. The machine is
//! this one: in place of `ssh`, a program runs the system's own SFTP
//! server, which speaks on its input and output as it does under `sshd`.
#![cfg(unix)]

// What two trees hold, compared: shared with the tests of the peer crate.
#[path = "../../peer/tests/common/mod.rs"]
mod common;

use common::assert_same_tree;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_mux");

/// Where systems keep the server that `sshd` runs for SFTP.
const SERVERS: [&str; 4] = [
    "/usr/libexec/sftp-server",
    "/usr/lib/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
];

/// A folder to work in, with a stand-in for `ssh` in it.
struct Trial {
    tmp: tempfile::TempDir,
}

impl Trial {
    /// `None` on a system that has no SFTP server to stand for the other
    /// machine: a Linux system without an SSH server. macOS always has one.
    fn new() -> Option<Self> {
        let server = SERVERS.iter().find(|path| Path::new(path).exists());
        #[cfg(target_os = "macos")]
        let server = Some(server.expect("macOS has an sftp-server"));
        let Some(server) = server else {
            eprintln!("no sftp-server on this system: nothing was run");
            return None;
        };
        let tmp = tempfile::tempdir().unwrap();
        // It notes how it was run, and then is the server.
        let noted = tmp.path().join("ran-as");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nexec {server}\n",
            noted.display()
        );
        let stand_in = tmp.path().join("ssh");
        std::fs::write(&stand_in, script).unwrap();
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        Some(Self { tmp })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().canonicalize().unwrap().join(name)
    }

    /// `mux`, with the stand-in for `ssh`, run in the trial's folder.
    fn mux(&self, args: &[&str]) -> Output {
        let ran = Command::new(BIN)
            .args(args)
            .env("MUX_SSH", self.tmp.path().join("ssh"))
            .current_dir(self.tmp.path())
            .output();
        ran.unwrap()
    }

    /// How the stand-in was run, each time.
    fn ran_as(&self) -> Vec<String> {
        let noted = std::fs::read_to_string(self.tmp.path().join("ran-as")).unwrap_or_default();
        noted.lines().map(str::to_owned).collect()
    }

    /// A small site at `site` in the trial's folder, and where it is as the
    /// other machine has it.
    fn site(&self) -> String {
        let site = self.path("site");
        std::fs::create_dir_all(site.join("assets/empty")).unwrap();
        std::fs::write(site.join("index.html"), b"<h1>hello</h1>\n").unwrap();
        let bytes: Vec<u8> = (0..2_500_000u32)
            .map(|i| (i ^ (i >> 9)).to_le_bytes()[0])
            .collect();
        std::fs::write(site.join("assets/big.bin"), bytes).unwrap();
        std::fs::write(site.join("assets/note.txt"), b"a note").unwrap();
        std::os::unix::fs::symlink("assets/big.bin", site.join("latest")).unwrap();
        let private = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(site.join("assets/note.txt"), private).unwrap();
        on_the_second(&site);
        format!("server:{}", site.display())
    }
}

/// Sets when everything at and under `path` was last changed to a whole
/// second. SFTP carries times to the second and no finer, so a copy has
/// them so, and is the same as what it was copied from only if that has
/// them so too.
fn on_the_second(path: &Path) {
    let found = std::fs::symlink_metadata(path).unwrap();
    if found.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            on_the_second(&entry.unwrap().path());
        }
    }
    if !found.is_symlink() {
        let whole = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::open(path)
            .unwrap()
            .set_modified(whole)
            .unwrap();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_folder_is_copied_from_a_machine_reached_by_ssh() {
    let Some(trial) = Trial::new() else { return };
    let site = trial.site();

    let copied = trial.mux(&["cp", &site, "out/"]);
    assert!(copied.status.success(), "{}", text(&copied.stderr));
    let line = text(&copied.stdout);
    assert!(line.trim_end().ends_with("\t2500021\tconsistent"), "{line}");
    assert_same_tree(&trial.path("site"), &trial.path("out/site"));
    // `ssh` was asked for the SFTP subsystem of that host, with nothing
    // after the host that it could take for an option.
    let ran = trial.ran_as();
    assert!(ran[0].ends_with("-s -- server sftp"), "{ran:?}");

    // Run again, nothing is read: every file is there with its size and
    // its time.
    let again = trial.mux(&["cp", &site, "out/"]);
    assert!(
        text(&again.stdout).trim_end().ends_with("\talready there"),
        "{}",
        text(&again.stdout)
    );

    // One file by itself, under a name of its own.
    let one = format!("{site}/assets/note.txt");
    let named = trial.mux(&["cp", &one, "mine.txt"]);
    assert!(named.status.success(), "{}", text(&named.stderr));
    assert_eq!(std::fs::read(trial.path("mine.txt")).unwrap(), b"a note");
}

#[test]
fn a_folder_is_synced_from_a_machine_reached_by_ssh() {
    let Some(trial) = Trial::new() else { return };
    let site = trial.site();
    let first = trial.mux(&["sync", &site, "mirror"]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_same_tree(&trial.path("site"), &trial.path("mirror"));

    // The other machine changes: a file is rewritten, one goes, one comes.
    std::fs::write(
        trial.path("site/index.html"),
        b"<h1>changed, and longer</h1>\n",
    )
    .unwrap();
    std::fs::remove_file(trial.path("site/assets/note.txt")).unwrap();
    std::fs::write(trial.path("site/assets/new.txt"), b"new").unwrap();
    on_the_second(&trial.path("site/assets"));
    // Its time is another, as that of a file that was rewritten is.
    let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_500);
    let rewritten = std::fs::File::open(trial.path("site/index.html")).unwrap();
    rewritten.set_modified(later).unwrap();
    let top = std::fs::File::open(trial.path("site")).unwrap();
    top.set_modified(later).unwrap();

    let planned = trial.mux(&["sync", &site, "mirror", "--delete", "--dry-run"]);
    let plan = text(&planned.stdout);
    for step in [
        "add\tassets/new.txt",
        "remove\tassets/note.txt",
        "replace\tindex.html",
    ] {
        assert!(plan.contains(step), "{plan}");
    }
    // The large file is as it was, and is not among what is fetched.
    assert!(!plan.contains("big.bin"), "{plan}");

    let synced = trial.mux(&["sync", &site, "mirror", "--delete", "--no-trash"]);
    assert!(synced.status.success(), "{}", text(&synced.stderr));
    assert_same_tree(&trial.path("site"), &trial.path("mirror"));
}

#[test]
fn a_place_on_another_machine_is_looked_at_and_its_port_is_passed_on() {
    let Some(trial) = Trial::new() else { return };
    let site = trial.site();
    let path = site.strip_prefix("server:").unwrap();
    let probed = trial.mux(&["probe", &format!("ssh://me@server:2222{path}")]);
    assert!(probed.status.success(), "{}", text(&probed.stderr));
    let said = text(&probed.stdout);
    for line in [
        "name     site",
        "kind     folder",
        "files    3",
        "parts    read several at once",
    ] {
        assert!(said.contains(line), "{said}");
    }
    let ran = trial.ran_as();
    assert!(ran[0].ends_with("-p 2222 -s -- me@server sftp"), "{ran:?}");
}

#[test]
fn what_cannot_be_copied_from_another_machine_says_why() {
    let Some(trial) = Trial::new() else { return };
    let site = trial.site();

    // Nothing is at the path.
    let absent = trial.mux(&["cp", "server:/no/such/place", "out/"]);
    assert_eq!(absent.status.code(), Some(3), "{}", text(&absent.stderr));
    assert!(text(&absent.stderr).contains("nothing is at server:/no/such/place"));

    // Copying to another machine is not there yet: refused as it is typed.
    let to = trial.mux(&["cp", "ssh", "server:/tmp/"]);
    assert_eq!(to.status.code(), Some(2), "{}", text(&to.stderr));
    assert!(
        text(&to.stderr).contains("not there yet"),
        "{}",
        text(&to.stderr)
    );

    // A digest is for a source on the web.
    let hash = format!("sha256:{}", "ab".repeat(32));
    let hashed = trial.mux(&["cp", &site, "out/", "--hash", &hash]);
    assert_eq!(hashed.status.code(), Some(2), "{}", text(&hashed.stderr));

    // The machine cannot be reached: what `ssh` said is passed on, and the
    // command ends as one that is worth running again.
    let refuses =
        "#!/bin/sh\necho 'ssh: connect to host server port 22: Connection refused' >&2\nexit 255\n";
    std::fs::write(trial.tmp.path().join("ssh"), refuses).unwrap();
    let unreachable = trial.mux(&["cp", &site, "out/"]);
    let why = text(&unreachable.stderr);
    assert_eq!(unreachable.status.code(), Some(4), "{why}");
    assert!(
        why.contains(
            "cannot reach server: ssh: connect to host server port 22: Connection refused"
        ),
        "{why}"
    );
    assert!(!trial.path("out").exists());
}
