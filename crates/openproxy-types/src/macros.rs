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
        $crate::impl_string_enum! {
            @impl
            meta: [$(#[$meta])*],
            vis: $vis,
            name: $name,
            variants: [
                $(
                    [$(#[$var_meta])*] $variant => $str $(| $alias)*
                ),*
            ],
            err_type: std::string::String,
            err_map: (|s: &str| format!("invalid {}: {s}", $err_label)),
            from_str_res: std::result::Result<$name, std::string::String>
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
        $crate::impl_string_enum! {
            @impl
            meta: [$(#[$meta])*],
            vis: $vis,
            name: $name,
            variants: [
                $(
                    [$(#[$var_meta])*] $variant => $str $(| $alias)*
                ),*
            ],
            err_type: $crate::error::CoreError,
            err_map: (|s: &str| $crate::error::CoreError::Validation(format!("invalid {}: {s}", $err_label))),
            from_str_res: $crate::error::Result<$name>
        }
    };
    (
        @impl
        meta: [$(#[$meta:meta])*],
        vis: $vis:vis,
        name: $name:ident,
        variants: [
            $(
                [$(#[$var_meta:meta])*] $variant:ident => $str:literal $(| $alias:literal)*
            ),* $(,)?
        ],
        err_type: $err_ty:ty,
        err_map: ($err_fn:expr),
        from_str_res: $res_ty:ty
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

            pub fn parse(s: &str) -> $res_ty {
                match s {
                    $(
                        $str $(| $alias)* => Ok(Self::$variant),
                    )*
                    other => Err(($err_fn)(other)),
                }
            }

            #[inline]
            #[allow(dead_code)]
            pub fn from_db_opt(s: Option<&str>) -> Option<Self> {
                s.and_then(|v| Self::parse(v).ok())
            }
        }

        impl std::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $err_ty;

            #[inline]
            fn from_str(s: &str) -> $res_ty {
                Self::parse(s)
            }
        }
    };
}

/// Trait for decoding string-backed enums and config types from database column values.
pub trait FromDb: Sized {
    fn from_db(s: Option<&str>) -> Self;
    fn from_db_opt(s: Option<&str>) -> Option<Self>;
}

impl<T> FromDb for T
where
    T: Default + std::str::FromStr,
{
    #[inline]
    fn from_db(s: Option<&str>) -> Self {
        s.and_then(|v| v.parse().ok()).unwrap_or_default()
    }

    #[inline]
    fn from_db_opt(s: Option<&str>) -> Option<Self> {
        s.and_then(|v| v.parse().ok())
    }
}

/// Implements `From<T>` conversions for enum variants, optionally applying
/// a mapping function or expression.
#[macro_export]
macro_rules! impl_enum_from {
    (
        $target:ident {
            $(
                $variant:ident($source:ty $(=> $map:expr)?)$(,)?
            )*
        }
    ) => {
        $(
            impl From<$source> for $target {
                #[inline]
                fn from(v: $source) -> Self {
                    $crate::impl_enum_from!(@apply v, $target, $variant $(, $map)?)
                }
            }
        )*
    };
    (@apply $val:ident, $target:ident, $variant:ident) => {
        $target::$variant($val)
    };
    (@apply $val:ident, $target:ident, $variant:ident, $map:expr) => {
        $target::$variant(($map)($val))
    };
}

#[cfg(test)]
mod tests {
    #[derive(Debug, PartialEq)]
    enum TestEnum {
        Text(String),
        Num(u32),
    }

    impl_enum_from! {
        TestEnum {
            Text(String),
            Text(&str => ToString::to_string),
            Num(u32),
        }
    }

    #[test]
    fn test_impl_enum_from() {
        let a: TestEnum = "hello".into();
        assert_eq!(a, TestEnum::Text("hello".to_string()));

        let b: TestEnum = String::from("world").into();
        assert_eq!(b, TestEnum::Text("world".to_string()));

        let c: TestEnum = 42u32.into();
        assert_eq!(c, TestEnum::Num(42));
    }

    impl_string_enum! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum StringErrorEnum {
            Alpha => "alpha" | "a",
            Beta => "beta",
        }
        error: "string_error_enum"
    }

    #[test]
    fn test_impl_string_enum_error_branch() {
        use std::str::FromStr;

        // as_str & Display
        assert_eq!(StringErrorEnum::Alpha.as_str(), "alpha");
        assert_eq!(StringErrorEnum::Beta.as_str(), "beta");
        assert_eq!(format!("{}", StringErrorEnum::Alpha), "alpha");
        assert_eq!(format!("{}", StringErrorEnum::Beta), "beta");

        // parse with main string and alias
        assert_eq!(StringErrorEnum::parse("alpha"), Ok(StringErrorEnum::Alpha));
        assert_eq!(StringErrorEnum::parse("a"), Ok(StringErrorEnum::Alpha));
        assert_eq!(StringErrorEnum::parse("beta"), Ok(StringErrorEnum::Beta));

        // FromStr
        assert_eq!(
            StringErrorEnum::from_str("alpha"),
            Ok(StringErrorEnum::Alpha)
        );
        assert_eq!("a".parse::<StringErrorEnum>(), Ok(StringErrorEnum::Alpha));
        assert_eq!("beta".parse::<StringErrorEnum>(), Ok(StringErrorEnum::Beta));

        // error case
        let err = StringErrorEnum::parse("unknown").unwrap_err();
        assert_eq!(err, "invalid string_error_enum: unknown");
        let from_str_err = "bad_val".parse::<StringErrorEnum>().unwrap_err();
        assert_eq!(from_str_err, "invalid string_error_enum: bad_val");
    }

    impl_string_enum! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum CoreErrorEnum {
            First => "first" | "1",
            Second => "second"
        }
        core_error: "core_error_enum"
    }

    #[test]
    fn test_impl_string_enum_core_error_branch() {
        use crate::error::CoreError;
        use std::str::FromStr;

        // as_str & Display
        assert_eq!(CoreErrorEnum::First.as_str(), "first");
        assert_eq!(CoreErrorEnum::Second.as_str(), "second");
        assert_eq!(format!("{}", CoreErrorEnum::First), "first");
        assert_eq!(format!("{}", CoreErrorEnum::Second), "second");

        // parse with main string and alias
        assert_eq!(CoreErrorEnum::parse("first").unwrap(), CoreErrorEnum::First);
        assert_eq!(CoreErrorEnum::parse("1").unwrap(), CoreErrorEnum::First);
        assert_eq!(
            CoreErrorEnum::parse("second").unwrap(),
            CoreErrorEnum::Second
        );

        // FromStr
        assert_eq!(
            CoreErrorEnum::from_str("first").unwrap(),
            CoreErrorEnum::First
        );
        assert_eq!("1".parse::<CoreErrorEnum>().unwrap(), CoreErrorEnum::First);
        assert_eq!(
            "second".parse::<CoreErrorEnum>().unwrap(),
            CoreErrorEnum::Second
        );

        // CoreError validation error
        let err = CoreErrorEnum::parse("bad").unwrap_err();
        match err {
            CoreError::Validation(msg) => assert_eq!(msg, "invalid core_error_enum: bad"),
            other => panic!("expected Validation error, got {other:?}"),
        }

        let from_str_err = "bad2".parse::<CoreErrorEnum>().unwrap_err();
        match from_str_err {
            CoreError::Validation(msg) => assert_eq!(msg, "invalid core_error_enum: bad2"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    impl_string_enum! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub enum DefaultableEnum {
            #[default]
            Standard => "standard",
            Enhanced => "enhanced",
        }
        error: "defaultable_enum"
    }

    #[test]
    fn test_impl_string_enum_from_db() {
        use super::FromDb;

        assert_eq!(
            DefaultableEnum::from_db(Some("enhanced")),
            DefaultableEnum::Enhanced
        );
        assert_eq!(
            DefaultableEnum::from_db(Some("unknown")),
            DefaultableEnum::Standard
        );
        assert_eq!(DefaultableEnum::from_db(None), DefaultableEnum::Standard);

        assert_eq!(
            DefaultableEnum::from_db_opt(Some("enhanced")),
            Some(DefaultableEnum::Enhanced)
        );
        assert_eq!(DefaultableEnum::from_db_opt(Some("unknown")), None);
        assert_eq!(DefaultableEnum::from_db_opt(None), None);
    }
}
