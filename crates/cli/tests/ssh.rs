//! Drives the real binary against a machine reached by SSH. The machine is
//! this one: in place of `ssh`, a program runs the system's own SFTP
//! server, which speaks on its input and output as it does under `sshd`.
#![cfg(unix)]

// What two trees hold, compared: shared with the tests of the peer crate.
#[path = "../../peer/tests/common/mod.rs"]
mod common;
// Stopping a command dead: shared with the other tests of the binary,
// which use more of it.
#[allow(dead_code, reason = "one of the things in it is not used here")]
mod kills;

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

    // What a ticket names is not copied on to another machine yet, and
    // nothing that goes to another machine can be offered from this one:
    // both are refused as they are typed, before anything is reached.
    let reached = trial.ran_as().len();
    let ticket = format!(
        "mux://{}?secret={}&peer={}",
        "ab".repeat(32),
        "cd".repeat(16),
        "ef".repeat(32)
    );
    let on = trial.mux(&["cp", &ticket, "server:/tmp/"]);
    assert_eq!(on.status.code(), Some(2), "{}", text(&on.stderr));
    assert!(
        text(&on.stderr).contains("not there yet"),
        "{}",
        text(&on.stderr)
    );
    let offered = trial.mux(&["cp", "ssh", "server:/tmp/", "--seed"]);
    assert_eq!(offered.status.code(), Some(2), "{}", text(&offered.stderr));
    assert_eq!(trial.ran_as().len(), reached, "{:?}", trial.ran_as());

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

/// A folder is copied to a machine reached by SSH as it is copied into a
/// folder of this one: everything in it, with times and permissions, under
/// part names until each file is whole. Run again it sends nothing, and
/// after a file is lost there it sends that file.
#[test]
fn a_folder_is_copied_to_a_machine_reached_by_ssh() {
    let Some(trial) = Trial::new() else { return };
    trial.site();
    // A folder that is not there yet, written as one: it is made.
    let there = trial.path("there/inbox");
    let to = format!("server:{}/", there.display());

    let copied = trial.mux(&["cp", "site", &to]);
    assert!(copied.status.success(), "{}", text(&copied.stderr));
    let line = text(&copied.stdout);
    assert_eq!(line.trim_end(), format!("{to}site\t2500021\tconsistent"));
    assert_same_tree(&trial.path("site"), &there.join("site"));
    let ran = trial.ran_as();
    assert!(ran[0].ends_with("-s -- server sftp"), "{ran:?}");

    let again = trial.mux(&["cp", "site", &to]);
    assert!(
        text(&again.stdout).trim_end().ends_with("\talready there"),
        "{}",
        text(&again.stdout)
    );

    // One file is lost there, and what an earlier try might have left is
    // beside another. A program is told of the one file that is sent.
    std::fs::remove_file(there.join("site/assets/note.txt")).unwrap();
    let stale = there.join("site/index.html.mux-part");
    std::fs::write(&stale, b"left by an earlier try").unwrap();
    let mended = trial.mux(&["cp", "--json", "site", &to]);
    assert!(mended.status.success(), "{}", text(&mended.stderr));
    let said = text(&mended.stdout);
    let lines: Vec<serde_json::Value> = said
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let named: Vec<&str> = lines
        .iter()
        .filter(|line| line["event"] == "file")
        .map(|line| line["path"].as_str().unwrap())
        .collect();
    assert_eq!(named, [format!("{to}site/assets/note.txt")]);
    let done = lines.last().unwrap();
    assert_eq!(
        (&done["fetched"], &done["kept"]),
        (&6.into(), &2_500_015.into())
    );
    assert_same_tree(&trial.path("site"), &there.join("site"));
    assert!(!stale.exists(), "what an earlier try left was kept");

    // Under a name of its own, where nothing has that name. And one file,
    // into a folder that is there.
    let renamed = format!("server:{}", there.join("copy").display());
    let named = trial.mux(&["cp", "site", &renamed]);
    assert!(named.status.success(), "{}", text(&named.stderr));
    assert_same_tree(&trial.path("site"), &there.join("copy"));
    let one = trial.mux(&["cp", "site/assets/note.txt", &renamed]);
    assert!(one.status.success(), "{}", text(&one.stderr));
    assert_eq!(
        std::fs::read(there.join("copy/note.txt")).unwrap(),
        b"a note"
    );

    // A dry run says what would be sent, and makes nothing there.
    let new = format!("server:{}/", trial.path("there/other").display());
    let dry = trial.mux(&["cp", "--dry-run", "site", &new]);
    assert!(dry.status.success(), "{}", text(&dry.stderr));
    assert_eq!(
        text(&dry.stdout).trim_end(),
        format!("{new}site\t2500021\tconsistent")
    );
    assert!(!trial.path("there/other").exists());
}

/// A copy to another machine that is stopped dead at random moments, twice
/// over, leaves no file with holes under a final name there, and the copy
/// that is then let finish leaves what the source holds.
#[test]
fn a_copy_to_another_machine_killed_at_any_moment_leaves_whole_files_and_can_be_finished() {
    const ROUNDS: usize = 4;
    let Some(trial) = Trial::new() else { return };
    let tree = trial.path("tree");
    let mut wanted: Vec<(String, Vec<u8>)> = Vec::new();
    for number in 0..24u32 {
        // A few large files among many small ones, so that a copy is
        // stopped both in the middle of a file and between files.
        let len = if number % 6 == 0 { 2_300_000 } else { 9_000 } + number;
        let bytes: Vec<u8> = (0..len)
            .map(|i| {
                (i.wrapping_mul(2_654_435_761) >> 13).to_le_bytes()[0] ^ number.to_le_bytes()[0]
            })
            .collect();
        let path = format!("d{}/f{number}", number % 4);
        std::fs::create_dir_all(tree.join(&path).parent().unwrap()).unwrap();
        std::fs::write(tree.join(&path), &bytes).unwrap();
        wanted.push((path, bytes));
    }
    on_the_second(&tree);
    let to = |name: &str| format!("server:{}/", trial.path(name).display());

    // How long an undisturbed copy takes here: the second one, which does
    // not pay for reading the program from disk.
    let mut whole = std::time::Duration::ZERO;
    for name in ["warm", "timed"] {
        let started = std::time::Instant::now();
        let done = trial.mux(&["cp", "tree", &to(name)]);
        assert!(done.status.success(), "{}", text(&done.stderr));
        whole = started.elapsed();
    }

    let mut moments = kills::Moments::new(ROUNDS, whole);
    while let Some(round) = moments.round() {
        let name = format!("there-{round}");
        let there = trial.path(&name).join("tree");
        for kill in 0..2 {
            let after = moments.next();
            let mut copy = Command::new(BIN);
            copy.args(["cp", "tree", &to(&name)])
                .env("MUX_SSH", trial.tmp.path().join("ssh"))
                .current_dir(trial.tmp.path())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            kills::kill(&mut copy, after);
            let mut all = true;
            for (path, bytes) in &wanted {
                match std::fs::read(there.join(path)) {
                    Ok(found) => assert!(
                        found == *bytes,
                        "round {round}, killed after {after:?}: {path} has its name and is not whole"
                    ),
                    Err(_) => all = false,
                }
            }
            if kill == 0 {
                moments.first_kill(!all);
            }
        }
        let finished = trial.mux(&["cp", "tree", &to(&name)]);
        assert!(
            finished.status.success(),
            "round {round}: {}",
            text(&finished.stderr)
        );
        assert_same_tree(&tree, &there);
    }
}
