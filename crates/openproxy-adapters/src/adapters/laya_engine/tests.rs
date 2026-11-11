use super::*;

#[cfg(test)]
mod safety_tests {
    use super::*;

    #[test]
    fn ffi_error_is_bounded_even_without_nul() {
        assert_eq!(ffi_error(b"abc"), "abc");
        assert_eq!(ffi_error(b"abc\0hidden"), "abc");
        assert_eq!(ffi_error(&[]), "");
        assert!(ffi_error(&[0xff]).contains('\u{fffd}'));
    }

    #[test]
    fn config_rejects_invalid_budgets_and_temperatures() {
        assert!(validate_config(&LayaConfig::default()).is_ok());
        for max_len in [0, 31, 8193, usize::MAX] {
            assert!(
                validate_config(&LayaConfig {
                    max_len,
                    ..Default::default()
                })
                .is_err()
            );
        }
        assert!(
            validate_config(&LayaConfig {
                temperature: vec![f32::NAN],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_config(&LayaConfig {
                head_max_len: 512,
                ..Default::default()
            })
            .is_err()
        );
    }
}
