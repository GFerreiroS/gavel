use cluster_core::ClusterError;
use thiserror::Error;

/// The storage failure type is defined in `cluster-core` so that the runtime
/// can use it too; the application layer knows it under this name.
pub use cluster_core::persist::StoreError as RepoError;

pub type AppResult<T> = Result<T, AppError>;
pub type RepoResult<T> = Result<T, RepoError>;

/// Every source string an error can put in front of a visitor.
///
/// Collected here rather than written inline at the point of failure, for two
/// reasons: the `app-web` i18n test walks this list to prove every one of them
/// is in the catalogue, and an error message is exactly the string a visitor
/// reads at the worst moment to be handed English.
pub mod text {
    pub const NOT_FOUND: &str = "not found";
    pub const UNAUTHORIZED: &str = "invalid username or password";
    pub const FORBIDDEN: &str = "not permitted";
    pub const ALREADY_EXISTS: &str = "that already exists";
    pub const INVALID_REQUEST: &str = "that request was not valid: {}";
    /// What a 5xx says. The real reason went to the log.
    pub const INTERNAL: &str = "Something went wrong on our side.";

    pub const USERNAME_LENGTH: &str = "username must be {}-{} characters";
    pub const USERNAME_CHARSET: &str = "username may contain letters, digits, '_' and '-' only";
    pub const PASSWORD_LENGTH: &str = "password must be {}-{} characters";
    pub const USERNAME_TAKEN: &str = "username already taken";

    /// A market annotation an administrator is writing down (§16, Phase 8).
    pub const EVENT_NEEDS_A_TITLE: &str = "an event needs a title";
    pub const EVENT_NEEDS_A_DATE: &str = "an event needs a date, as YYYY-MM-DD";
    pub const TOO_MANY_SIGN_INS: &str = "too many sign-in attempts; try again in {} minutes";
    pub const TOO_MANY_SIGN_UPS: &str = "too many new accounts just now; try again in {} minutes";

    pub const TASK_COUNT_RANGE: &str = "task count must be between 1 and {}";
    pub const SLEEP_RANGE: &str = "sleep duration must be between 1 and {} ms";
    pub const PRIME_RANGE: &str = "prime bound must be between 2 and {}";
    pub const UNKNOWN_JOB_KIND: &str = "unknown job kind '{}'";
    pub const UNKNOWN_ROLE: &str = "unknown role '{}'";

    pub const REGION_REQUIRED: &str = "a region is required";
    pub const REGION_CHARSET: &str = "that region name contains characters that are not allowed";
    pub const REALM_REQUIRED: &str = "a realm is required";
    pub const REALM_CHARSET: &str = "that realm name contains characters that are not allowed";
    pub const CHARACTER_REQUIRED: &str = "a character name is required";
    pub const CHARACTER_CHARSET: &str =
        "that character name contains characters that are not allowed";

    pub const DISCORD_WEBHOOK_INVALID: &str = "that doesn't look like a Discord webhook URL (should start with https://discord.com/api/webhooks/)";

    /// The whole list, for the test that says none of them can go untranslated.
    pub const ALL: &[&str] = &[
        NOT_FOUND,
        UNAUTHORIZED,
        FORBIDDEN,
        ALREADY_EXISTS,
        INVALID_REQUEST,
        INTERNAL,
        USERNAME_LENGTH,
        USERNAME_CHARSET,
        PASSWORD_LENGTH,
        USERNAME_TAKEN,
        TOO_MANY_SIGN_INS,
        TOO_MANY_SIGN_UPS,
        TASK_COUNT_RANGE,
        SLEEP_RANGE,
        PRIME_RANGE,
        UNKNOWN_JOB_KIND,
        UNKNOWN_ROLE,
        REGION_REQUIRED,
        REGION_CHARSET,
        REALM_REQUIRED,
        REALM_CHARSET,
        CHARACTER_REQUIRED,
        CHARACTER_CHARSET,
        DISCORD_WEBHOOK_INVALID,
    ];
}

/// Something a visitor will read: a source string, and the values to put in it.
///
/// Not a sentence that has already been assembled. By the time an error
/// reaches `IntoResponse` there is no locale left to assemble one in, and an
/// error built with `format!` can never be anything but English -- which is
/// how "invalid username or password" ended up as the one part of a fully
/// translated sign-in page that was not translated.
///
/// `{}` placeholders are filled in order, the same convention the
/// `|t|fill(..)` template filter uses, so a translation is free to move them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub source: &'static str,
    pub args: Vec<String>,
}

impl Message {
    pub fn new(source: &'static str) -> Self {
        Self {
            source,
            args: Vec::new(),
        }
    }

    pub fn with(
        source: &'static str,
        args: impl IntoIterator<Item = impl std::fmt::Display>,
    ) -> Self {
        Self {
            source,
            args: args.into_iter().map(|a| a.to_string()).collect(),
        }
    }

    /// Fill this message's arguments into `sentence`, which is the source
    /// string in whatever language it was looked up in.
    pub fn render(&self, sentence: &str) -> String {
        let mut out = sentence.to_string();
        for arg in &self.args {
            out = out.replacen("{}", arg, 1);
        }
        out
    }
}

impl std::fmt::Display for Message {
    /// The English rendering. Used by the `Error` implementation and by logs;
    /// a page renders the translated source instead.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render(self.source))
    }
}

/// Application-level failure. `app-web` maps this onto HTTP status codes; it
/// is never constructed from a raw SQLx or reqwest error directly.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Validation(Message),
    /// Deliberately the same error for "no such user" and "wrong password",
    /// so the response cannot be used to enumerate accounts.
    #[error("invalid username or password")]
    Unauthorized,
    #[error("not permitted")]
    Forbidden,
    /// Too many attempts too quickly. Carries its own message because the
    /// only thing worth saying is how long the caller has to wait.
    #[error("{0}")]
    TooManyRequests(Message),
    #[error("{0}")]
    Conflict(Message),
    #[error("upstream provider: {0}")]
    Integration(String),
    #[error(transparent)]
    Cluster(#[from] ClusterError),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn validation(source: &'static str) -> Self {
        AppError::Validation(Message::new(source))
    }

    pub fn validation_with(
        source: &'static str,
        args: impl IntoIterator<Item = impl std::fmt::Display>,
    ) -> Self {
        AppError::Validation(Message::with(source, args))
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        AppError::Internal(msg.into())
    }

    /// The HTTP status this error should produce. Kept here so the mapping is
    /// defined once, next to the variants.
    pub fn status_code(&self) -> u16 {
        match self {
            AppError::NotFound => 404,
            AppError::Validation(_) => 400,
            AppError::Unauthorized => 401,
            AppError::Forbidden => 403,
            AppError::TooManyRequests(_) => 429,
            AppError::Conflict(_) => 409,
            AppError::Integration(_) => 502,
            AppError::Cluster(ClusterError::UnknownNode(_))
            | AppError::Cluster(ClusterError::UnknownJob(_))
            | AppError::Cluster(ClusterError::UnknownTask(_)) => 404,
            AppError::Cluster(ClusterError::Invalid(_)) => 400,
            AppError::Repo(RepoError::NotFound) => 404,
            AppError::Repo(RepoError::Conflict(_)) => 409,
            _ => 500,
        }
    }

    /// Whether the detail is safe to show a user, or should be logged only.
    pub fn is_public(&self) -> bool {
        self.status_code() < 500
    }

    /// What to put on the page, as a source string plus its values.
    ///
    /// Every arm returns something from [`text`], so every arm is
    /// translatable. The diagnostic text a 5xx carries -- an upstream error, a
    /// failed query -- never appears here: it goes to the log, and the visitor
    /// gets a sentence in their own language.
    pub fn message(&self) -> Message {
        match self {
            AppError::NotFound
            | AppError::Repo(RepoError::NotFound)
            | AppError::Cluster(
                ClusterError::UnknownNode(_)
                | ClusterError::UnknownJob(_)
                | ClusterError::UnknownTask(_),
            ) => Message::new(text::NOT_FOUND),
            AppError::Unauthorized => Message::new(text::UNAUTHORIZED),
            AppError::Forbidden => Message::new(text::FORBIDDEN),
            AppError::Validation(m) | AppError::Conflict(m) | AppError::TooManyRequests(m) => {
                m.clone()
            }
            AppError::Repo(RepoError::Conflict(_)) => Message::new(text::ALREADY_EXISTS),
            // The cluster's own complaint about a submitted job. The sentence
            // around it translates; the detail is the cluster's wording.
            AppError::Cluster(ClusterError::Invalid(detail)) => {
                Message::with(text::INVALID_REQUEST, [detail])
            }
            _ => Message::new(text::INTERNAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_fill_placeholders_in_order() {
        let message = Message::with(text::USERNAME_LENGTH, [3, 32]);
        assert_eq!(message.to_string(), "username must be 3-32 characters");
    }

    /// A translation is free to reorder the sentence around its values, and
    /// the count of placeholders is what has to survive, not their position.
    #[test]
    fn a_translation_fills_its_own_placeholders() {
        let message = Message::with(text::USERNAME_LENGTH, [3, 32]);
        assert_eq!(
            message.render("el usuario debe tener entre {} y {} caracteres"),
            "el usuario debe tener entre 3 y 32 caracteres"
        );
    }

    /// A 5xx must never put its diagnostic on the page, whatever it says.
    #[test]
    fn internal_failures_say_nothing_but_the_generic_sentence() {
        for error in [
            AppError::internal("connection refused to 10.0.0.4:5432"),
            AppError::Integration("Battle.net token request failed: 401".into()),
        ] {
            assert!(!error.is_public());
            assert_eq!(error.message().source, text::INTERNAL);
            assert!(error.message().args.is_empty());
        }
    }

    #[test]
    fn a_storage_conflict_does_not_leak_the_constraint() {
        let error = AppError::Repo(RepoError::Conflict(
            "UNIQUE constraint failed: users.username".into(),
        ));
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.message().source, text::ALREADY_EXISTS);
    }

    /// Every source string an error can produce has to be in `text::ALL`, or
    /// the i18n coverage test cannot see it and it ships untranslated.
    #[test]
    fn every_message_source_is_listed() {
        let errors = [
            AppError::NotFound,
            AppError::Unauthorized,
            AppError::Forbidden,
            AppError::validation(text::USERNAME_CHARSET),
            AppError::Conflict(Message::new(text::USERNAME_TAKEN)),
            AppError::TooManyRequests(Message::with(text::TOO_MANY_SIGN_INS, [5])),
            AppError::Repo(RepoError::NotFound),
            AppError::Repo(RepoError::Conflict("x".into())),
            AppError::Cluster(ClusterError::Invalid("x".into())),
            AppError::internal("x"),
        ];
        for error in errors {
            let source = error.message().source;
            assert!(
                text::ALL.contains(&source),
                "error source not listed in text::ALL: {source:?}"
            );
        }
    }
}
