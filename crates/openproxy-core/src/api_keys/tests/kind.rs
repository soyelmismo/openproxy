use super::{fresh_pool, make_input};
use crate::api_keys::{crud, kind};
use crate::error::CoreError;

#[test]
fn a_key_created_by_the_checkout_is_a_customer_from_birth() {
    let (conn, _p) = fresh_pool();
    let (key, _plain) =
        crud::create(&conn, make_input("client"), "credits-checkout:ORD-42").unwrap();
    let rows = kind::list(&conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, key.id.0);
    assert_eq!(rows[0].kind, kind::KIND_CUSTOMER);
}

#[test]
fn a_manually_created_key_starts_personal() {
    let (conn, _p) = fresh_pool();
    let (key, _plain) = crud::create(&conn, make_input("mine"), "admin").unwrap();
    let rows = kind::list(&conn).unwrap();
    assert_eq!(rows[0].id, key.id.0);
    assert_eq!(rows[0].kind, kind::KIND_PERSONAL);
}

#[test]
fn set_flips_a_key_both_ways_and_list_reflects_it() {
    let (conn, _p) = fresh_pool();
    let (key, _plain) = crud::create(&conn, make_input("mine"), "admin").unwrap();

    assert_eq!(
        kind::set(&conn, key.id.0, "customer").unwrap(),
        kind::KIND_CUSTOMER
    );
    assert_eq!(kind::list(&conn).unwrap()[0].kind, kind::KIND_CUSTOMER);

    assert_eq!(
        kind::set(&conn, key.id.0, " personal ").unwrap(),
        kind::KIND_PERSONAL
    );
    assert_eq!(kind::list(&conn).unwrap()[0].kind, kind::KIND_PERSONAL);
}

#[test]
fn set_rejects_an_unknown_kind_without_touching_the_row() {
    let (conn, _p) = fresh_pool();
    let (key, _plain) = crud::create(&conn, make_input("mine"), "admin").unwrap();

    let err = kind::set(&conn, key.id.0, "vip").unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "got: {err:?}");
    // The stored kind survives a rejected write.
    assert_eq!(kind::list(&conn).unwrap()[0].kind, kind::KIND_PERSONAL);
}

#[test]
fn set_on_a_missing_key_is_not_found() {
    let (conn, _p) = fresh_pool();
    let err = kind::set(&conn, 9_999, "customer").unwrap_err();
    assert!(matches!(err, CoreError::NotFound { .. }), "got: {err:?}");
}
