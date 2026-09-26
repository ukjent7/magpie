use super::SavedLogin;

// live_login is ZCode's own account, read from ZCode's credential store;
// its key stays there and is never written to logins.json.
pub(super) fn live_login() -> Option<SavedLogin> {
    let own = crate::provider::zcode::own()?;
    Some(SavedLogin {
        agent: "zcode".to_owned(),
        user: own.user,
        ..SavedLogin::default()
    })
}
