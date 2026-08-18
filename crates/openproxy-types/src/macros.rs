//! Declarative macros for type implementations.

/// Implements parsing (`parse`), formatting (`Display`), conversion (`FromStr`),
/// and string representation (`as_str`) for string-backed enums.
#[macro_export]
macro_rules! impl_string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$var_meta:meta])*
                $variant:ident => $str:literal $(| $alias:literal)*
            ),* $(,)?
        }
        error: $err_label:literal
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $(
                $(#[$var_meta])*
                $variant,
            )*
        }

        impl $name {
            #[inline]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(
                        Self::$variant => $str,
                    )*
                }
            }

            pub fn parse(s: &str) -> std::result::Result<Self, String> {
                match s {
                    $(
                        $str $(| $alias)* => Ok(Self::$variant),
                    )*
                    other => Err(format!("invalid {}: {other}", $err_label)),
                }
            }
        }

        impl std::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            #[inline]
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$var_meta:meta])*
                $variant:ident => $str:literal $(| $alias:literal)*
            ),* $(,)?
        }
        core_error: $err_label:literal
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $(
                $(#[$var_meta])*
                $variant,
            )*
        }

        impl $name {
            #[inline]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(
                        Self::$variant => $str,
                    )*
                }
            }

            pub fn parse(s: &str) -> $crate::error::Result<Self> {
                match s {
                    $(
                        $str $(| $alias)* => Ok(Self::$variant),
                    )*
                    other => Err($crate::error::CoreError::Validation(format!(
                        "invalid {}: {other}",
                        $err_label
                    ))),
                }
            }
        }

        impl std::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::error::CoreError;

            #[inline]
            fn from_str(s: &str) -> $crate::error::Result<Self> {
                Self::parse(s)
            }
        }
    };
}
