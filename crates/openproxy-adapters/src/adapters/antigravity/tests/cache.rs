use super::*;

#[test]
fn antigravity_cache_session_es_estable_al_crecer_el_historial() {
    let initial = serde_json::json!([{"role":"user","parts":[{"text":"Consulta inicial"}]}]);
    let continued = serde_json::json!([
        {"role":"user","parts":[{"text":"Consulta inicial"}]},
        {"role":"model","parts":[{"text":"Respuesta"}]},
        {"role":"user","parts":[{"text":"Continúa"}]}
    ]);
    let other = serde_json::json!([{"role":"user","parts":[{"text":"Otra conversación"}]}]);
    let sid = generate_stable_antigravity_session_id(Some(&initial));
    assert_eq!(
        sid,
        generate_stable_antigravity_session_id(Some(&continued))
    );
    assert_ne!(sid, generate_stable_antigravity_session_id(Some(&other)));
}
