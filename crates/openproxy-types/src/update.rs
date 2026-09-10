/// Tri-state field for PATCH operations.
/// - `Ignore`: field absent from request body → no-op
/// - `Reset`: field present with null → clear to SQL NULL
/// - `Set(T)`: field present with value → set to T
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum UpdateField<T> {
    #[default]
    Ignore,
    Reset,
    Set(T),
}

impl<T> UpdateField<T> {
    /// Borrow the inner value.
    pub fn as_ref(&self) -> UpdateField<&T> {
        match self {
            Self::Ignore => UpdateField::Ignore,
            Self::Reset => UpdateField::Reset,
            Self::Set(v) => UpdateField::Set(v),
        }
    }

    /// Transform the inner value.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> UpdateField<U> {
        match self {
            Self::Ignore => UpdateField::Ignore,
            Self::Reset => UpdateField::Reset,
            Self::Set(v) => UpdateField::Set(f(v)),
        }
    }

    /// True if this is `Ignore`.
    pub fn is_ignore(&self) -> bool {
        matches!(self, Self::Ignore)
    }

    /// For DB binding: maps Ignore→None (caller skips), Reset→Some(None) (SQL NULL), Set(v)→Some(Some(v)).
    ///
    /// WARNING: caller MUST match Ignore first; calling this on Ignore returns None, same as
    /// `None` from `Reset`. Use pattern matching on `UpdateField` variants directly for
    /// unambiguous branching.
    pub fn into_option(self) -> Option<Option<T>> {
        match self {
            Self::Ignore => None,
            Self::Reset => Some(None),
            Self::Set(v) => Some(Some(v)),
        }
    }
}
