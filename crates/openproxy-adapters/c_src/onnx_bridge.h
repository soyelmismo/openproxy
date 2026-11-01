#ifndef ONNX_BRIDGE_H
#define ONNX_BRIDGE_H

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

/* ---------------- EmbeddingGemma 2 ---------------- */

/* Text/vision/audio encoders all project into one 768-dim space. The vision
 * and audio towers emit 512-dim features that the text backbone consumes as
 * extra inputs, so a session trio is needed for any multimodal request. */
#define EMBED_EMBEDDING_DIM 768
#define EMBED_FEATURE_DIM 512
#define EMBED_IMAGE_PATCHES 2520   /* 280 soft tokens * 3 * 3 */
#define EMBED_IMAGE_PATCH_PIXELS 768 /* patch_size^2 * channels = 16*16*3 */
#define EMBED_AUDIO_FEATURE_SIZE 128
#define EMBED_MAX_IMAGE_TOKENS 280
#define EMBED_MAX_AUDIO_TOKENS 750

typedef struct EmbedSession EmbedSession;
typedef struct VisionSession VisionSession;
typedef struct AudioSession AudioSession;

// Creates an ONNX runtime session for the EmbeddingGemma text backbone.
EmbedSession* embed_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
);
void embed_session_destroy(EmbedSession* session);

// Embedding dimension of the loaded model (0 if unknown).
int embed_session_embedding_dim(EmbedSession* session);

// Runs a batch of token sequences. The three feature pointers may be NULL when
// their row count is zero; otherwise they hold row-major [rows, 512] floats
// that must line up with the placeholder tokens inside the sequences.
// Writes batch_size * embedding_dim floats, already L2-normalized by the graph.
int embed_session_run(
    EmbedSession* session,
    int64_t batch_size,
    int64_t seq_len,
    const int64_t* input_ids,
    const int64_t* attention_mask,
    const float* image_features,
    int64_t image_feature_rows,
    const float* video_features,
    int64_t video_feature_rows,
    const float* audio_features,
    int64_t audio_feature_rows,
    float* out_embeddings,
    char* err_buf,
    size_t err_buf_len
);

// Vision tower: patchified pixels + axial position ids -> [rows, 512].
VisionSession* vision_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
);
void vision_session_destroy(VisionSession* session);

int vision_session_run(
    VisionSession* session,
    int64_t batch_size,
    int64_t num_patches,
    const float* pixel_values,
    const int64_t* position_ids,
    float* out_features,
    int64_t* out_rows,
    char* err_buf,
    size_t err_buf_len
);

// Audio tower: log-mel frames + validity mask -> [rows, 512].
AudioSession* audio_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
);
void audio_session_destroy(AudioSession* session);

int audio_session_run(
    AudioSession* session,
    int64_t batch_size,
    int64_t seq_len,
    const float* input_features,
    const uint8_t* input_features_mask,
    float* out_features,
    int64_t* out_rows,
    char* err_buf,
    size_t err_buf_len
);

#ifdef __cplusplus
}
#endif

#endif // ONNX_BRIDGE_H
