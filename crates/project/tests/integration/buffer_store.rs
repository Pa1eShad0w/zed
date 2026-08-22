//! How a file that is read-only on disk is opened.
//!
//! Perforce keeps every file that is not open for edit read-only on disk, so the on-disk
//! permission means something different there than it does under git.

use language::Capability;
use project::buffer_store::{
    PerforceCheckoutVerdict, capability_for_loaded_file, perforce_auto_checkout_enabled,
};
use project::project_settings::PerforceSettings;

#[test]
fn writable_file_always_opens_editable() {
    for verdict in [
        PerforceCheckoutVerdict::AutoCheckout,
        PerforceCheckoutVerdict::NotApplicable,
        PerforceCheckoutVerdict::Unknown,
    ] {
        assert_eq!(
            capability_for_loaded_file(true, verdict),
            Capability::ReadWrite,
            "a writable file must open editable regardless of the Perforce verdict"
        );
    }
}

#[test]
fn read_only_file_outside_perforce_opens_locked() {
    assert_eq!(
        capability_for_loaded_file(false, PerforceCheckoutVerdict::NotApplicable),
        Capability::Read,
        "git / non-VCS files keep upstream's read-only-on-disk behavior"
    );
}

#[test]
fn read_only_file_under_perforce_auto_checkout_opens_editable() {
    assert_eq!(
        capability_for_loaded_file(false, PerforceCheckoutVerdict::AutoCheckout),
        Capability::ReadWrite,
        "Perforce keeps unopened files read-only on disk; locking the tab would make the edit \
         that triggers the checkout impossible"
    );
}

#[test]
fn read_only_file_opens_locked_until_the_verdict_is_known() {
    assert_eq!(
        capability_for_loaded_file(false, PerforceCheckoutVerdict::Unknown),
        Capability::Read,
        "opening a file must never wait on Perforce, so an unknown verdict falls back to \
         upstream behavior and is settled in the background"
    );
}

#[test]
fn auto_checkout_gate_follows_the_perforce_settings() {
    assert!(
        perforce_auto_checkout_enabled(&PerforceSettings::default()),
        "auto-checkout is on by default"
    );
    assert!(
        !perforce_auto_checkout_enabled(&PerforceSettings {
            enabled: false,
            ..PerforceSettings::default()
        }),
        "with Perforce integration off, a read-only file really is read-only"
    );
    assert!(
        !perforce_auto_checkout_enabled(&PerforceSettings {
            edit_on_file_save: false,
            ..PerforceSettings::default()
        }),
        "without the pre-save checkout there is nothing to clear the read-only bit"
    );
}

/// Settling a buffer that parked before its repository existed.
///
/// A read-only file opened while repository discovery is still running has no repository to ask,
/// so it parks and waits. The `GitStore` is what later reports a repository — and it does so from
/// inside its own `update`, with the entity leased out of the entity map. Anything on that path
/// that reads the git store back aborts the process, so these tests pin the settle path down.
mod perforce_relock {
    use crate::Project;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn discovering_a_repository_settles_a_parked_read_only_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/project"), json!({ "a.txt": "hello" }))
            .await;
        fs.set_readonly(path!("/project/a.txt"), true);

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/a.txt"), cx)
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();

        project.read_with(cx, |project, cx| {
            assert_eq!(
                project
                    .buffer_store()
                    .read(cx)
                    .perforce_parked_buffers()
                    .len(),
                1,
                "a read-only file opened with no repository must park for a later decision"
            );
        });

        // Repository discovery reports the new repo from inside `GitStore::update`. Settling the
        // parked buffer there used to read the leased git store back and abort the process.
        fs.insert_tree(path!("/project/.git"), json!({})).await;
        cx.executor().run_until_parked();

        project.read_with(cx, |project, cx| {
            assert!(
                project
                    .buffer_store()
                    .read(cx)
                    .perforce_parked_buffers()
                    .is_empty(),
                "discovering a repository must settle the parked buffer"
            );
        });
    }
}
