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
