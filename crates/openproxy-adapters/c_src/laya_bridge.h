#ifndef LAYA_BRIDGE_H
#define LAYA_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct LayaSession LayaSession;

// Creates an ONNX runtime session for Laya.
// Returns a pointer to LayaSession on success, NULL on error (with error written to err_buf).
LayaSession* laya_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
);

// Destroys the session and frees all associated resources.
void laya_session_destroy(LayaSession* session);

// Runs inference on the Laya model.
// Returns 0 on success, non-zero on error (with error written to err_buf).
int laya_session_run(
    LayaSession* session,
    int64_t batch_size,
    int64_t seq_len,
    int64_t max_markers,
    const int64_t* input_ids,
    const int64_t* attention_mask,
    const int64_t* marker_pos,
    const uint8_t* marker_mask,
    const int64_t* qtype,
    float* out_logits,
    char* err_buf,
    size_t err_buf_len
);

#ifdef __cplusplus
}
#endif

#endif // LAYA_BRIDGE_H
