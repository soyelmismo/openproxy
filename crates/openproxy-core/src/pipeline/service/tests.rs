use crate::combos::{self, Strategy};
use crate::pipeline::service::*;
use crate::pipeline::test_utils::*;
use crate::pipeline::*;
use crate::secrets::MasterKey;
use std::sync::Arc;
use tower::Service;

#[tokio::test]
async fn test_resolve_service_resolves_combo() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let combo_id = {
        let writer = pool.writer();
        let combo_id =
            combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("create combo");
        seed_target_with_account(&writer, combo_id, "p", "m", Some("sk-test"), &mk, 1);
        combo_id
    };

    let cfg = test_config(mk);
    let pipeline = Pipeline::new(conn, cfg);

    // Prepare a mock leaf service that returns success.
    #[derive(Clone)]
    struct LeafService;
    impl tower::Service<PipelineState> for LeafService {
        type Response = PipelineResult;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<std::result::Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, state: PipelineState) -> Self::Future {
            assert!(state.combo.is_some());
            assert_eq!(state.combo.unwrap().name, "c");
            std::future::ready(Ok(PipelineResult {
                status_code: 200,
                error: None,
                final_response: None,
                attempts: 1,
                usage_tuple: None,
            }))
        }
    }

    let mut service = ResolveService::new(pipeline, LeafService);
    let (req, _dis_tx) = make_request(combo_id);
    let state = PipelineState {
        req,
        combo: None,
        eligible_targets: None,
        race_size: None,
    };

    let res = service.call(state).await.unwrap();
    println!("DEBUG res = {:?}", res);
    assert_eq!(res.status_code, 200);
}
