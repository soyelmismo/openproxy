use super::*;
use super::checkin::{SigninDayItem, SigninDayStatus, calculate_streak};
use super::matrix::MiniMaxRegion;
use super::md5::md5_hex;

#[test]
fn test_md5_rfc_vectors() {
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5_hex(b"message digest"),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );
}

#[test]
fn test_matrix_url_generation() {
    let (url, query) = matrix::build_gateway_url(
        MiniMaxRegion::Global,
        "/minimax-cloud/api/v1/signin/status",
        "user_999",
        1720000000000,
    );

    assert!(url.starts_with("https://agent.minimax.io/minimax-cloud/api/v1/signin/status?"));
    assert!(query.contains("user_id=user_999"));
    assert!(query.contains("client=mcode"));
    assert!(query.contains("unix=1720000000000"));
}

#[test]
fn test_provider_name_and_aliases() {
    let p = MiniMaxOAuthProvider::new();
    assert_eq!(p.name(), "minimax");
    assert!(p.aliases().contains(&"minimax-coding"));
    assert!(p.aliases().contains(&"minimax-cn"));
    assert_eq!(p.flow(), OAuthFlow::DeviceCode);
}

#[test]
fn test_checkin_streak_progression() {
    let days = vec![
        SigninDayItem {
            day_no: 1,
            points: 10,
            bonus_points: None,
            status: SigninDayStatus::Claimed as u8,
            is_today: false,
        },
        SigninDayItem {
            day_no: 2,
            points: 10,
            bonus_points: None,
            status: SigninDayStatus::Claimed as u8,
            is_today: false,
        },
        SigninDayItem {
            day_no: 3,
            points: 10,
            bonus_points: None,
            status: SigninDayStatus::Claimable as u8,
            is_today: true,
        },
        SigninDayItem {
            day_no: 4,
            points: 10,
            bonus_points: None,
            status: SigninDayStatus::Upcoming as u8,
            is_today: false,
        },
    ];

    // Day 1 & 2 are claimed, day 3 is today and claimable -> current streak is 2
    assert_eq!(calculate_streak(&days), 2);
}

#[test]
fn test_build_complete_verification_uri() {
    let uri = build_complete_verification_uri("https://account.minimax.io/oauth-authorize", "LRAR-BW2A");
    assert_eq!(
        uri,
        "https://account.minimax.io/oauth-authorize?user_code=LRAR-BW2A&client_surface=tui&download_source=mcode-internal"
    );

    let uri_cn = build_complete_verification_uri("https://account.minimax.cn/oauth-authorize?existing=1", "7TM4-GRFT");
    assert_eq!(
        uri_cn,
        "https://account.minimax.cn/oauth-authorize?existing=1&user_code=7TM4-GRFT&client_surface=tui&download_source=mcode-internal"
    );
}
