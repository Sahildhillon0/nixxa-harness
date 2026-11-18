//! Sign-in methods: flows that turn a browser sign-in into a stored credential (ARCHITECTURE §6).
//! Each is a concrete type the harness drives; none opens a browser or draws anything.
//!
//! Only providers that officially allow third-party apps to sign users in get a method
//! (D-017). Stage 3 has one: OpenRouter's OAuth PKCE flow.

mod loopback;
pub mod openrouter;
mod pkce;

/// Why a sign-in didn't produce a credential. Never contains a code, verifier or key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    /// The local callback listener couldn't start.
    #[error("couldn't start the sign-in listener: {0}")]
    Listener(String),
    /// Nobody came back from the browser in time.
    #[error("sign-in timed out; start it again")]
    TimedOut,
    /// The user (or the provider) said no.
    #[error("sign-in didn't finish: {0}")]
    Denied(String),
    /// The provider couldn't be reached.
    #[error("couldn't reach the provider: {0}")]
    Network(String),
    /// The provider refused to turn the code into a key.
    #[error("the provider didn't issue a key: {0}")]
    Exchange(String),
}
