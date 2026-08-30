use crate::PipelineRequest;
use openproxy_types::combos::ComboTarget;
use openproxy_types::models::Model;

pub(crate) fn is_horde_vision_request(
    target: &ComboTarget,
    model: &Model,
    req: &PipelineRequest,
) -> bool {
    target.provider_id.as_str() == "horde"
        && (openproxy_adapters::HordeAdapter::is_vision_model(model.model_id.as_str())
            || openproxy_adapters::HordeAdapter::extract_image_from_messages(
                &req.openai_request.messages,
            )
            .is_some())
}
