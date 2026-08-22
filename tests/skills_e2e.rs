//! E2E skills-subystem tests: discovery across workspace roots, the
//! `/skills` command harness (create/install/remove/show/list), and the
//! `skill`/`install_skill` tool surface.
//!
//! Uses a unique FX_HOME per test so the managed root never touches the real
//! profile. Tests that mutate FX_HOME must not run concurrently.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use fxrs::skills::{discover, managed_root, Registry};

static COUNTER: AtomicU32 = AtomicU32::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fxrs-skills-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("FX_HOME", &dir);
    dir
}

fn write(path: &PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn make_skill(dir: &Path, name: &str, description: &str) {
    write(&dir.join(name).join("SKILL.md"), &format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nInstructions for {name}...\n"
    ));
}

#[test]
fn discovery_and_command_harness_end_to_end() {
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let workspace = home.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    // Workspace skill (auto-discovered from .fx/skills).
    make_skill(
        &workspace.join(".fx/skills"),
        "review",
        "Review pull requests",
    );

    let catalog = discover(&workspace);
    let review = catalog.find("review").expect("workspace skill discovered");
    assert_eq!(review.description, "Review pull requests");
    assert!(!review.managed_install);

    // Create a managed skill through the command harness.
    let cmd = fxrs::skills::commands::parse_command("create helper");
    let result = fxrs::skills::commands::execute_command(&workspace, &cmd).unwrap();
    assert!(
        result.render().contains("helper/SKILL.md"),
        "{}",
        result.render()
    );
    assert!(managed_root().join("helper/SKILL.md").is_file());

    // The managed root is discovered.
    let catalog = discover(&workspace);
    assert!(catalog.find("helper").is_some());
    assert!(catalog.find("helper").unwrap().managed_install);

    // Install from a local directory of skills.
    let repo = home.join("repo");
    make_skill(&repo, "alpha", "Alpha helper");
    make_skill(&repo, "beta", "Beta helper");
    let install = fxrs::skills::commands::parse_command(&format!("install {}", repo.display()));
    let result = fxrs::skills::commands::execute_command(&workspace, &install).unwrap();
    assert!(result.render().contains("alpha"), "{}", result.render());
    assert!(result.render().contains("beta"), "{}", result.render());
    assert!(managed_root().join("alpha/SKILL.md").is_file());
    assert!(managed_root().join("beta/SKILL.md").is_file());

    // Show: reads back the SKILL.md body.
    let show = fxrs::skills::commands::parse_command("show review");
    let result = fxrs::skills::commands::execute_command(&workspace, &show).unwrap();
    assert!(
        result.render().contains("Instructions for review"),
        "{}",
        result.render()
    );

    // Remove only works for managed skills.
    let remove_alpha = fxrs::skills::commands::parse_command("remove alpha");
    let result = fxrs::skills::commands::execute_command(&workspace, &remove_alpha).unwrap();
    assert!(
        result.render().contains("Removed skill 'alpha'"),
        "{}",
        result.render()
    );
    assert!(!managed_root().join("alpha").exists());

    let remove_review = fxrs::skills::commands::parse_command("remove review");
    let result = fxrs::skills::commands::execute_command(&workspace, &remove_review).unwrap();
    assert!(
        result.render().contains("not the fx managed install root"),
        "{}",
        result.render()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn registry_resolves_skills_for_tool_lookup() {
    let _guard = SERIAL.lock().unwrap();
    let home = temp_home();
    let workspace = home.join("ws2");
    std::fs::create_dir_all(&workspace).unwrap();
    make_skill(&workspace.join(".fx/skills"), "tooling", "Tooling skill");

    let registry = Registry::discover(&workspace);
    assert!(registry.find("tooling").is_some());
    assert!(registry.find("missing").is_none());

    let _ = std::fs::remove_dir_all(&home);
}
