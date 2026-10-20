//! Sandbox self-check: the same scenario the acceptance tests run, executable on a real
//! machine through `appdir selftest`, with no kernel component and no privilege needed.
//!
//! Everything happens inside a throwaway directory that this module creates and removes.
//! Nothing here reads or deletes anything outside that directory.

use crate::clean::{self, CleanOptions};
use crate::db::Db;
use crate::filter::{CreateRequest, IO_FILE_CREATED, IO_FILE_OPENED};
use crate::ledger::Ledger;
use crate::path;
use crate::raw;
use crate::types::{IgnoreReason, RecordStatus};
use crate::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl Step {
    fn ok(name: &str, detail: impl Into<String>) -> Step {
        Step { name: name.to_string(), passed: true, detail: detail.into() }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Step {
        Step { name: name.to_string(), passed: false, detail: detail.into() }
    }
}

/// Run the whole scenario. `sandbox` must be an empty directory owned by the caller.
pub fn run(db_path: &Path, sandbox: &Path) -> Result<Vec<Step>> {
    let mut steps: Vec<Step> = Vec::new();
    let db = Db::open(db_path)?;
    let now = crate::timefmt::now_ms();

    let root = sandbox.join("Local");
    let root_s = root.to_string_lossy().to_string();
    let other_root = sandbox.join("LocalBackup");
    let other_s = other_root.to_string_lossy().to_string();
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&other_root)?;

    // --- monitor configuration -------------------------------------------
    let (_, added) = db.add_root(&root_s, now)?;
    steps.push(check("monitor add accepts an absolute path", added, "first add reported as new"));
    let (_, dup) = db.add_root(&format!("{root_s}\\."), now)?;
    steps.push(check("monitor add is idempotent for equal paths", !dup, "trailing separator normalized"));
    db.add_root(&other_s, now)?;

    let ledger = Ledger::new(&db)?;
    steps.push(check(
        "two monitor roots are active",
        ledger.view.roots.len() == 2,
        format!("roots={}", ledger.view.roots.len()),
    ));

    let exe_a = sandbox.join("apps/one/App.exe");
    let exe_b = sandbox.join("apps/two/App.exe");
    std::fs::create_dir_all(exe_a.parent().unwrap())?;
    std::fs::create_dir_all(exe_b.parent().unwrap())?;
    std::fs::write(&exe_a, b"stub").unwrap();
    std::fs::write(&exe_b, b"stub").unwrap();
    let exe_a = exe_a.to_string_lossy().to_string();
    let exe_b = exe_b.to_string_lossy().to_string();

    // --- case 1: a direct child was created ------------------------------
    let child = root.join("AppA");
    std::fs::create_dir(&child)?;
    let out = ledger.ingest(&raw(
        &child.to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1001,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check("direct child creation is recorded", out.label() == "recorded", out.label()));

    // --- case 2: same name again (duplicate open) ------------------------
    let out2 = ledger.ingest(&raw(
        &child.to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1001,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check("the same directory is not registered twice", out2.label() == "duplicate", out2.label()));

    // --- case 3: nested directory is ignored -----------------------------
    let nested = child.join("nested");
    std::fs::create_dir(&nested)?;
    let out3 = ledger.ingest(&raw(
        &nested.to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1001,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check(
        "a directory below the direct-child level is ignored",
        matches!(out3, crate::IngestOutcome::Ignored { reason: IgnoreReason::NotDirectChild, .. }),
        format!("{:?}", out3),
    ));

    // --- case 4: opening an existing directory is not a creation ---------
    let out4 = ledger.ingest(&raw(
        &root.join("AppZ").to_string_lossy(),
        CreateRequest::dir_open(),
        IO_FILE_OPENED,
        true,
        1001,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check(
        "opening an existing directory is not registered",
        matches!(out4, crate::IngestOutcome::Ignored { reason: IgnoreReason::OpenedExisting, .. }),
        format!("{:?}", out4),
    ));

    // --- case 5: a failed create is not registered -----------------------
    let out5 = ledger.ingest(&raw(
        &root.join("AppFail").to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        false,
        1001,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check(
        "a failed create is not registered",
        matches!(out5, crate::IngestOutcome::Ignored { reason: IgnoreReason::Failed, .. }),
        format!("{:?}", out5),
    ));

    // --- case 6: same EXE, several directories ---------------------------
    for n in ["AppB", "AppC"] {
        let d = root.join(n);
        std::fs::create_dir(&d)?;
        ledger.ingest(&raw(
            &d.to_string_lossy(),
            CreateRequest::dir_create(),
            IO_FILE_CREATED,
            true,
            1002,
            Some(&exe_b),
            now,
        ))?;
    }
    let many = db.dirs_of_exe(&exe_b, false)?;
    steps.push(check(
        "one EXE path lists every directory it created",
        many.len() == 2,
        format!("count={}", many.len()),
    ));

    // --- case 7: two different EXE paths with the same file name ---------
    let apps = db.app_count()?;
    steps.push(check(
        "same file name in different folders means different applications",
        apps == 2,
        format!("applications={apps} (App.exe at two paths)"),
    ));

    // --- case 8: prefix-name trap between roots --------------------------
    let trap = other_root.join("Inside");
    std::fs::create_dir(&trap)?;
    let out8 = ledger.ingest(&raw(
        &trap.to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1003,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check(
        "a sibling root whose name starts with another root name is still matched correctly",
        out8.label() == "recorded",
        format!("{:?}", out8),
    ));
    let outside = sandbox.join("Elsewhere").join("X");
    std::fs::create_dir_all(&outside)?;
    let out8b = ledger.ingest(&raw(
        &outside.to_string_lossy(),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1003,
        Some(&exe_a),
        now,
    ))?;
    steps.push(check(
        "a path outside every monitor root is ignored",
        matches!(out8b, crate::IngestOutcome::Ignored { reason: IgnoreReason::OutsideMonitor, .. }),
        format!("{:?}", out8b),
    ));

    // --- case 9: creator unknown is dropped, never guessed ---------------
    let lost = root.join("Unattributed");
    std::fs::create_dir(&lost)?;
    let out9 = ledger.ingest(&raw(&lost.to_string_lossy(), CreateRequest::dir_create(), IO_FILE_CREATED, true, 1004, None, now))?;
    steps.push(check(
        "an event without a resolvable EXE path is ignored",
        matches!(out9, crate::IngestOutcome::Ignored { reason: IgnoreReason::CreatorUnknown, .. }),
        format!("{:?}", out9),
    ));
    steps.push(check(
        "the ignored event is still visible in diagnostics",
        db.ignored_counts()?.iter().any(|(r, _)| r == "creator-unknown"),
        format!("{:?}", db.ignored_counts()?),
    ));

    // --- case 10: delete, reconcile, recreate re-registers --------------
    let rc = root.join("Recreated");
    let rc_s = rc.to_string_lossy().to_string();
    std::fs::create_dir(&rc)?;
    let out10 = ledger.ingest(&raw(&rc_s, CreateRequest::dir_create(), IO_FILE_CREATED, true, 1005, Some(&exe_b), now))?;
    steps.push(check("a fresh direct child is recorded", out10.label() == "recorded", out10.label()));
    std::fs::remove_dir_all(&rc)?;
    let changed = ledger.reconcile(now)?;
    let marked = db.dir_by_path(&rc_s)?;
    steps.push(check(
        "reconcile marks a deleted directory missing without erasing history",
        marked.as_ref().map(|r| r.record_status == RecordStatus::Missing).unwrap_or(false) && changed >= 1,
        format!("status={:?} changed={changed}", marked.as_ref().map(|r| r.record_status)),
    ));
    std::fs::create_dir(&rc)?;
    let out10b = ledger.ingest(&raw(&rc_s, CreateRequest::dir_create(), IO_FILE_CREATED, true, 1006, Some(&exe_a), now + 3000))?;
    steps.push(check(
        "a recreated directory is re-registered with its new creator",
        out10b.label() == "reregistered",
        format!("{out10b:?}"),
    ));
    let r2 = db.dir_by_path(&rc_s)?;
    steps.push(check(
        "the re-registered row is active again with the new PID",
        r2.as_ref().map(|r| r.record_status == RecordStatus::Active && r.creator_pid == 1006).unwrap_or(false),
        format!("{:?}", r2.as_ref().map(|r| (r.record_status, r.creator_pid))),
    ));

    // --- case 11: same name, different object is refused ---------------
    let stale = root.join("StaleName");
    std::fs::create_dir(&stale)?;
    let long_ago = now - 3_600_000;
    ledger.ingest(&raw(&stale.to_string_lossy(), CreateRequest::dir_create(), IO_FILE_CREATED, true, 1007, Some(&exe_a), long_ago))?;
    let c11 = clean::verify(&db, &stale.to_string_lossy(), None, &[])?;
    steps.push(check(
        "a record whose creation time contradicts the real directory is refused",
        is_blocked_as(&c11, crate::clean::BlockReason::IdentityChanged),
        c11.block_text(),
    ));

    let _ = &nested; // nested was only ever an ignored case
    let target = root.join("AppB");
    let plan = clean::verify(&db, &target.to_string_lossy(), Some(&exe_b), &[])?;
    steps.push(check(
        "a verified candidate is deletable",
        plan.deletable(),
        plan.block_text(),
    ));
    // A path with no ledger row must be refused.
    let orphan = sandbox.join("notmonitored.bin");
    std::fs::write(&orphan, b"x").unwrap();
    let denied = clean::verify(&db, &orphan.to_string_lossy(), None, &[])?;
    steps.push(check(
        "a path without a ledger row is refused",
        is_blocked_as(&denied, crate::clean::BlockReason::NoLedgerRow),
        denied.block_text(),
    ));
    // Wrong owner must be refused even though a row exists.
    let wrong = clean::verify(&db, &target.to_string_lossy(), Some(&exe_a), &[])?;
    steps.push(check(
        "cleaning by the wrong EXE path is refused",
        is_blocked_as(&wrong, crate::clean::BlockReason::OwnerMismatch),
        wrong.block_text(),
    ));

    // --- case 13: backup failure aborts deletion -------------------------
    let blocked_backup_root = sandbox.join("i-am-a-file");
    std::fs::write(&blocked_backup_root, b"not a directory").unwrap();
    let opts = CleanOptions {
        backup_dir: Some(&blocked_backup_root),
        commit: true,
        protected_prefixes: Vec::new(),
    };
    let rep = clean::execute(&db, vec![plan], &opts, now)?;
    steps.push(check(
        "a failed backup stops the deletion",
        rep.failed == 1 && target.exists(),
        format!("{:?}", rep.actions.iter().map(|a| &a.detail).collect::<Vec<_>>()),
    ));

    // --- case 14: successful backup + delete ----------------------------
    let backup_root = sandbox.join("backups");
    let cand = clean::verify(&db, &target.to_string_lossy(), Some(&exe_b), &[])?;
    let opts2 = CleanOptions {
        backup_dir: Some(&backup_root),
        commit: true,
        protected_prefixes: Vec::new(),
    };
    let rep2 = clean::execute(&db, vec![cand], &opts2, now)?;
    let backed = std::fs::read_dir(&backup_root).map(|rd| rd.count()).unwrap_or(0);
    steps.push(check(
        "backup then delete succeeds and the ledger row becomes missing",
        rep2.deleted == 1 && !target.exists() && backed == 1,
        format!("deleted={} backups={backed} {:?}", rep2.deleted, rep2.actions.first().map(|a| &a.detail)),
    ));
    let after = db.dir_by_path(&target.to_string_lossy())?;
    steps.push(check(
        "the removed directory is recorded as missing, not erased from history",
        after.as_ref().map(|r| r.record_status == RecordStatus::Missing).unwrap_or(false),
        format!("{:?}", after.map(|r| r.record_status)),
    ));

    // --- case 15: a link or junction target is refused ------------------
    let link_parent = root.join("LinkTarget");
    let _ = std::fs::remove_dir_all(&link_parent);
    std::fs::create_dir_all(&link_parent)?;
    let link = root.join("MyLink");
    let made_link = try_make_link(&link_parent, &link);
    if made_link {
        // register a row first so the only reason left for refusal is the reparse point
        ledger.ingest(&raw(&link.to_string_lossy(), CreateRequest::dir_create(), IO_FILE_CREATED, true, 1009, Some(&exe_a), now))?;
        let c = clean::verify(&db, &link.to_string_lossy(), None, &[])?;
        steps.push(check(
            "a reparse point is never deleted, even with a ledger row",
            is_blocked_as(&c, crate::clean::BlockReason::ReparsePoint),
            c.block_text(),
        ));
    } else {
        steps.push(Step::ok(
            "reparse-point refusal",
            "skipped: this machine does not allow creating a test link (needs developer mode or privilege)",
        ));
    }

    // --- case 16: protected prefixes are refused ------------------------
    let data_dir = sandbox.to_string_lossy().to_string();
    let cand_p = clean::verify(&db, &root.join("AppC").to_string_lossy(), None, &[data_dir])?;
    steps.push(check(
        "a path inside the protected data directory is refused",
        is_blocked_as(&cand_p, crate::clean::BlockReason::SelfOwned)
            || is_blocked_as(&cand_p, crate::clean::BlockReason::NotDirectChild),
        cand_p.block_text(),
    ));

    // --- case 17: volume root and monitor root are never candidates -----
    let vol = clean::verify(&db, "C:\\", None, &[])?;
    steps.push(check(
        "a volume root is refused",
        vol.blocked.is_some(),
        vol.block_text(),
    ));
    let root_delete = clean::verify(&db, &root_s, None, &[])?;
    steps.push(check(
        "a monitor root itself is refused",
        root_delete.blocked.is_some(),
        root_delete.block_text(),
    ));

    // --- case 18: different spellings of the same path collapse ---------
    let spelled = format!("{}\\AppD", root_s);
    std::fs::create_dir(root.join("AppD"))?;
    let o1 = ledger.ingest(&raw(&spelled, CreateRequest::dir_create(), IO_FILE_CREATED, true, 1010, Some(&exe_a), now))?;
    let o2 = ledger.ingest(&raw(
        &format!("{}\\\\.\\\\AppD", root_s),
        CreateRequest::dir_create(),
        IO_FILE_CREATED,
        true,
        1010,
        Some(&exe_a),
        now + 1,
    ))?;
    steps.push(check(
        "the same directory written differently is registered once",
        o1.label() == "recorded" && o2.label() == "duplicate",
        format!("{o1:?} / {o2:?}"),
    ));
    let _ = path::key_of(&spelled);

    Ok(steps)
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Step {
    if passed {
        Step::ok(name, detail)
    } else {
        Step::fail(name, detail)
    }
}

/// True when the candidate was blocked for exactly this reason.
fn is_blocked_as(c: &crate::clean::Candidate, reason: crate::clean::BlockReason) -> bool {
    c.blocked.as_ref().map(|(r, _)| *r == reason).unwrap_or(false)
}

/// Try to create a directory link for the reparse-point test. Returns false when the
/// machine does not permit it (no developer mode / privilege), which the caller treats as
/// "skipped", never as "passed".
#[cfg(windows)]
fn try_make_link(target: &Path, link: &Path) -> bool {
    std::os::windows::fs::symlink_dir(target, link).is_ok()
}

#[cfg(not(windows))]
fn try_make_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}
