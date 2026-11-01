#define _GNU_SOURCE
#include "onnx_bridge.h"
#include "onnxruntime_c_api.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#define DL_OPEN(path) (void*)LoadLibraryA(path)
#define DL_SYM(handle, sym) (void*)GetProcAddress((HMODULE)(handle), sym)
#define DL_CLOSE(handle) FreeLibrary((HMODULE)(handle))
#define DL_ERROR() "LoadLibrary failed"
#else
#include <dlfcn.h>
#include <unistd.h>
#define DL_OPEN(path) dlopen(path, RTLD_NOW | RTLD_GLOBAL)
#define DL_SYM(handle, sym) dlsym(handle, sym)
#define DL_CLOSE(handle) dlclose(handle)
#define DL_ERROR() "failed to load dynamic library"
#endif

static void set_error(char* err_buf, size_t err_buf_len, const char* msg) {
    if (err_buf && err_buf_len > 0) {
        snprintf(err_buf, err_buf_len, "%s", msg);
    }
}

static const char* candidate_paths[] = {
    "/usr/local/lib/libonnxruntime.so",
    "/usr/lib/libonnxruntime.so",
    "/usr/lib/x86_64-linux-gnu/libonnxruntime.so",
    "/usr/lib/aarch64-linux-gnu/libonnxruntime.so",
    "/opt/homebrew/lib/libonnxruntime.dylib",
    "/usr/local/lib/libonnxruntime.dylib",
#ifdef _WIN32
    "C:\\onnxruntime\\onnxruntime.dll",
    "onnxruntime.dll",
#endif
    NULL
};

/* Security (OP-12): the ONNX library is loaded INTO the isolated worker via
 * dlopen/LoadLibrary. Bare relative names ("libonnxruntime.so") resolved
 * through LD_LIBRARY_PATH and the current working directory, letting anyone
 * who controls either inject code directly into the process. Only explicit
 * absolute paths are accepted. */
static int is_absolute_lib_path(const char* path) {
    if (!path || path[0] == '\0') return 0;
#ifdef _WIN32
    if (path[0] == '\\' || (path[0] != '\0' && path[1] == ':')) return 1;
    /* Fall back to the classic DLL search for the bare Windows name kept in
     * candidate_paths; every other name must be absolute. */
    return strcmp(path, "onnxruntime.dll") == 0;
#else
    return path[0] == '/';
#endif
}

/* ---------- shared runtime handle ---------- */

/* Every session in this file needs the same five ORT objects. Opening them
 * once here keeps the dlopen policy, the API version pin and the teardown
 * order in a single place instead of once per model. */
typedef struct OnnxHandle {
    void* dl_handle;
    const OrtApi* ort;
    OrtEnv* env;
    OrtSessionOptions* opts;
    OrtSession* session;
    OrtMemoryInfo* mem_info;
} OnnxHandle;

static void onnx_handle_close(OnnxHandle* h);

static OnnxHandle* onnx_handle_open(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    const char* env_name,
    char* err_buf,
    size_t err_buf_len
) {
    void* handle = NULL;
    const char* env_path = getenv("OPENPROXY_ONNX_LIB");

    if (onnx_lib_path && onnx_lib_path[0] != '\0') {
        if (!is_absolute_lib_path(onnx_lib_path)) {
            set_error(err_buf, err_buf_len,
                      "onnx lib path must be absolute (relative paths would "
                      "resolve through LD_LIBRARY_PATH/CWD)");
            return NULL;
        }
        handle = DL_OPEN(onnx_lib_path);
    }
    if (!handle && env_path && env_path[0] != '\0') {
        if (!is_absolute_lib_path(env_path)) {
            set_error(err_buf, err_buf_len, "OPENPROXY_ONNX_LIB must be an absolute path");
            return NULL;
        }
        handle = DL_OPEN(env_path);
    }
    if (!handle) {
        for (int i = 0; candidate_paths[i] != NULL; ++i) {
            handle = DL_OPEN(candidate_paths[i]);
            if (handle) break;
        }
    }
    if (!handle) {
        set_error(err_buf, err_buf_len, DL_ERROR());
        return NULL;
    }

    OrtApiBase* (*get_api_base)(void) = (OrtApiBase* (*)(void))DL_SYM(handle, "OrtGetApiBase");
    if (!get_api_base) {
        set_error(err_buf, err_buf_len, "dlsym / GetProcAddress OrtGetApiBase failed");
        DL_CLOSE(handle);
        return NULL;
    }
    const OrtApiBase* base = get_api_base();
    if (!base) {
        set_error(err_buf, err_buf_len, "OrtGetApiBase returned NULL");
        DL_CLOSE(handle);
        return NULL;
    }
    /* Use API version 20 (compatible with ORT 1.20+). */
    const OrtApi* ort = base->GetApi(20);
    if (!ort) {
        set_error(err_buf, err_buf_len, "OrtApi version 20 not supported by runtime");
        DL_CLOSE(handle);
        return NULL;
    }

    OnnxHandle* h = (OnnxHandle*)calloc(1, sizeof(OnnxHandle));
    if (!h) {
        set_error(err_buf, err_buf_len, "out of memory allocating ONNX handle");
        DL_CLOSE(handle);
        return NULL;
    }
    h->dl_handle = handle;
    h->ort = ort;

    OrtStatus* status = ort->CreateEnv(ORT_LOGGING_LEVEL_WARNING, env_name, &h->env);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        onnx_handle_close(h);
        return NULL;
    }

    status = ort->CreateSessionOptions(&h->opts);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        onnx_handle_close(h);
        return NULL;
    }
    if (num_threads > 0) {
        ort->SetIntraOpNumThreads(h->opts, num_threads);
    }
    ort->SetSessionGraphOptimizationLevel(h->opts, ORT_ENABLE_ALL);

#ifdef _WIN32
    wchar_t w_model_path[1024];
    MultiByteToWideChar(CP_UTF8, 0, model_path, -1, w_model_path, 1024);
    status = ort->CreateSession(h->env, w_model_path, h->opts, &h->session);
#else
    status = ort->CreateSession(h->env, model_path, h->opts, &h->session);
#endif
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        onnx_handle_close(h);
        return NULL;
    }

    status = ort->CreateCpuMemoryInfo(OrtArenaAllocator, OrtMemTypeDefault, &h->mem_info);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        onnx_handle_close(h);
        return NULL;
    }

    return h;
}

/* Releases the ONNX Runtime resources a handle owns. Deliberately does NOT
 * free `h`: callers pass either an owning heap pointer (error paths in
 * onnx_handle_open) or the address of a handle embedded as the first member
 * of a session struct. `OnnxHandle` is the first field of every session, so
 * &session->handle == session — freeing through it here would make the
 * caller's own free(session) a double free. */
static void onnx_handle_release(OnnxHandle* h) {
    if (!h) return;
    if (h->ort) {
        if (h->mem_info) h->ort->ReleaseMemoryInfo(h->mem_info);
        if (h->session) h->ort->ReleaseSession(h->session);
        if (h->opts) h->ort->ReleaseSessionOptions(h->opts);
        if (h->env) h->ort->ReleaseEnv(h->env);
    }
    if (h->dl_handle) DL_CLOSE(h->dl_handle);
    h->mem_info = NULL;
    h->session = NULL;
    h->opts = NULL;
    h->env = NULL;
    h->dl_handle = NULL;
}

/* Frees an owning heap handle. Only for handles allocated by onnx_handle_open
 * on its success path; session structs use onnx_handle_release + free. */
static void onnx_handle_close(OnnxHandle* h) {
    if (!h) return;
    onnx_handle_release(h);
    free(h);
}

/* Wraps a caller-owned buffer as an OrtValue. The caller keeps ownership of
 * the memory; only the OrtValue wrapper is released afterwards. */
static OrtStatus* wrap_tensor(
    const OrtApi* ort,
    OrtMemoryInfo* mem_info,
    const void* data,
    size_t byte_len,
    const int64_t* dims,
    size_t num_dims,
    ONNXTensorElementDataType type,
    OrtValue** out
) {
    return ort->CreateTensorWithDataAsOrtValue(
        mem_info, (void*)data, byte_len, dims, num_dims, type, out);
}

/* Validates a 2-D float output against the expected row count and width,
 * then copies it out. Guards against a replaced model declaring a smaller
 * tensor, which would otherwise make the memcpy read past the buffer. */
static int copy_f32_output(
    const OrtApi* ort,
    OrtValue* value,
    int64_t expected_rows,
    int64_t expected_width,
    int dynamic_rows,
    const char* what,
    float* out,
    int64_t* out_rows,
    char* err_buf,
    size_t err_buf_len
) {
    OrtTensorTypeAndShapeInfo* shape_info = NULL;
    ONNXTensorElementDataType element_type = ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
    size_t element_count = 0;
    size_t rank = 0;
    int64_t dimensions[2] = {0, 0};
    OrtStatus* status = ort->GetTensorTypeAndShape(value, &shape_info);
    if (!status) status = ort->GetTensorElementType(shape_info, &element_type);
    if (!status) status = ort->GetTensorShapeElementCount(shape_info, &element_count);
    if (!status) status = ort->GetDimensionsCount(shape_info, &rank);
    if (!status && rank == 2) status = ort->GetDimensions(shape_info, dimensions, 2);
    if (shape_info) ort->ReleaseTensorTypeAndShapeInfo(shape_info);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -9;
    }
    if (element_type != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT || rank != 2 ||
        dimensions[1] != expected_width ||
        element_count != (size_t)dimensions[0] * (size_t)expected_width ||
        (!dynamic_rows && dimensions[0] != expected_rows)) {
        char msg[224];
        snprintf(msg, sizeof(msg),
                 "%s tensor mismatch: type=%d rank=%zu shape=[%lld,%lld], expected [%s,%lld] FLOAT",
                 what, (int)element_type, rank,
                 (long long)dimensions[0], (long long)dimensions[1],
                 dynamic_rows ? "*" : "rows", (long long)expected_width);
        set_error(err_buf, err_buf_len, msg);
        return -10;
    }
    if (dynamic_rows && out_rows) *out_rows = dimensions[0];

    void* data = NULL;
    status = ort->GetTensorMutableData(value, &data);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -11;
    }
    memcpy(out, data, element_count * sizeof(float));
    return 0;
}

/* ---------------- */
/* Laya             */
/* ---------------- */

struct LayaSession {
    OnnxHandle handle;
};

LayaSession* laya_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
) {
    OnnxHandle* h = onnx_handle_open(onnx_lib_path, model_path, num_threads,
                                     "laya_engine", err_buf, err_buf_len);
    if (!h) return NULL;
    LayaSession* s = (LayaSession*)calloc(1, sizeof(LayaSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating LayaSession");
        onnx_handle_close(h);
        return NULL;
    }
    s->handle = *h;
    free(h);
    return s;
}

void laya_session_destroy(LayaSession* session) {
    if (!session) return;
    onnx_handle_release(&session->handle);
    free(session);
}

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
) {
    if (!session || !session->handle.ort || !session->handle.session ||
        !session->handle.mem_info) {
        set_error(err_buf, err_buf_len, "invalid session");
        return -1;
    }
    if (batch_size <= 0 || batch_size > 64 || seq_len <= 0 || seq_len > 8192 ||
        max_markers <= 0 || max_markers > 128 || !input_ids || !attention_mask ||
        !marker_pos || !marker_mask || !qtype || !out_logits) {
        set_error(err_buf, err_buf_len, "invalid or oversized inference buffers");
        return -1;
    }

    const OrtApi* ort = session->handle.ort;
    OrtValue* in_vals[5] = {NULL, NULL, NULL, NULL, NULL};
    OrtValue* out_vals[1] = {NULL};

    int64_t dims_seq[2] = {batch_size, seq_len};
    int64_t dims_markers[2] = {batch_size, max_markers};
    int64_t dims_qtype[1] = {batch_size};

    struct {
        const void* data;
        size_t byte_len;
        const int64_t* dims;
        size_t num_dims;
        ONNXTensorElementDataType type;
    } inputs[5] = {
        {input_ids, (size_t)(batch_size * seq_len * sizeof(int64_t)), dims_seq, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
        {attention_mask, (size_t)(batch_size * seq_len * sizeof(int64_t)), dims_seq, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
        {marker_pos, (size_t)(batch_size * max_markers * sizeof(int64_t)), dims_markers, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
        {marker_mask, (size_t)(batch_size * max_markers * sizeof(uint8_t)), dims_markers, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_BOOL},
        {qtype, (size_t)(batch_size * sizeof(int64_t)), dims_qtype, 1, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
    };

    for (int i = 0; i < 5; ++i) {
        OrtStatus* status = wrap_tensor(ort, session->handle.mem_info, inputs[i].data,
                                        inputs[i].byte_len, inputs[i].dims,
                                        inputs[i].num_dims, inputs[i].type, &in_vals[i]);
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            for (int j = 0; j < i; ++j) ort->ReleaseValue(in_vals[j]);
            return -(i + 2);
        }
    }

    const char* input_names[5] = {"input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"};
    const char* output_names[1] = {"logits"};

    OrtStatus* status = ort->Run(
        session->handle.session,
        NULL,
        input_names,
        (const OrtValue* const*)in_vals,
        5,
        output_names,
        1,
        out_vals
    );

    for (int i = 0; i < 5; ++i) {
        ort->ReleaseValue(in_vals[i]);
    }

    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -7;
    }

    int rc = copy_f32_output(ort, out_vals[0], batch_size, max_markers, 0, "logits",
                             out_logits, NULL, err_buf, err_buf_len);
    ort->ReleaseValue(out_vals[0]);
    return rc;
}

/* ---------------- */
/* EmbeddingGemma text backbone */
/* ---------------- */

struct EmbedSession {
    OnnxHandle handle;
    int embedding_dim;
};

/* Read the declared feature width of a named graph output.
 *
 * The text tower exports both `last_hidden_state` (rank 3) and
 * `sentence_embedding` (rank 2), and `sentence_embedding` is NOT output 0 —
 * reading index 0 blindly reports rank 3 and yields no dimension. Resolve the
 * output by name, the same way embed_session_run requests it.
 *
 * Returns 0 when the name is absent or the shape is dynamic; the caller then
 * learns the dimension from the first real run instead of failing.
 */
static int discover_embedding_dim(const OrtApi* ort, OrtSession* session) {
    size_t count = 0;
    if (!ort || ort->SessionGetOutputCount(session, &count)) return 0;
    OrtAllocator* alloc = NULL;
    OrtStatus* alloc_status = ort->GetAllocatorWithDefaultOptions(&alloc);
    if (alloc_status || !alloc) {
        if (alloc_status) ort->ReleaseStatus(alloc_status);
        return 0;
    }

    int dim = 0;
    for (size_t i = 0; i < count && dim == 0; ++i) {
        char* name = NULL;
        if (ort->SessionGetOutputName(session, i, alloc, &name) || !name) continue;
        int is_target = strcmp(name, "sentence_embedding") == 0;
        alloc->Free(alloc, name);
        if (!is_target) continue;

        OrtTypeInfo* type_info = NULL;
        if (ort->SessionGetOutputTypeInfo(session, i, &type_info) || !type_info) continue;
        const OrtTensorTypeAndShapeInfo* shape_info = NULL;
        /* CastTypeInfoToTensorInfo returns a borrowed reference — do NOT
           release it (ReleaseTypeInfo below owns the allocation). */
        if (!ort->CastTypeInfoToTensorInfo(type_info, &shape_info) && shape_info) {
            size_t rank = 0;
            if (!ort->GetDimensionsCount(shape_info, &rank) && rank == 2) {
                int64_t dimensions[2] = {0, 0};
                if (!ort->GetDimensions(shape_info, dimensions, 2) && dimensions[1] > 0) {
                    dim = (int)dimensions[1];
                }
            }
        }
        ort->ReleaseTypeInfo(type_info);
    }
    return dim;
}

EmbedSession* embed_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
) {
    OnnxHandle* h = onnx_handle_open(onnx_lib_path, model_path, num_threads,
                                     "embed_engine", err_buf, err_buf_len);
    if (!h) return NULL;
    EmbedSession* s = (EmbedSession*)calloc(1, sizeof(EmbedSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating EmbedSession");
        onnx_handle_close(h);
        return NULL;
    }
    s->handle = *h;
    free(h);

    s->embedding_dim = discover_embedding_dim(s->handle.ort, s->handle.session);

    return s;
}

void embed_session_destroy(EmbedSession* session) {
    if (!session) return;
    onnx_handle_release(&session->handle);
    free(session);
}

int embed_session_embedding_dim(EmbedSession* session) {
    if (!session) return 0;
    return session->embedding_dim;
}

/* Optional inputs must be present even when empty: the graph declares them as
 * fixed names, so a zero-row [0, 512] tensor is passed instead of omitting
 * them. A non-NULL pointer keeps CreateTensorWithDataAsOrtValue happy. */
static const float empty_features[1] = {0.0f};

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
) {
    if (!session || !session->handle.ort || !session->handle.session ||
        !session->handle.mem_info) {
        set_error(err_buf, err_buf_len, "invalid session");
        return -1;
    }
    if (batch_size <= 0 || batch_size > 64 || seq_len <= 0 || seq_len > 8192 ||
        !input_ids || !attention_mask || !out_embeddings) {
        set_error(err_buf, err_buf_len, "invalid or oversized inference buffers");
        return -1;
    }
    /* The placeholders inside the sequences must be matched by exactly this
     * many feature rows, otherwise the graph silently misaligns modalities. */
    if (image_feature_rows < 0 || image_feature_rows > EMBED_MAX_IMAGE_TOKENS ||
        video_feature_rows < 0 || video_feature_rows > EMBED_MAX_IMAGE_TOKENS ||
        audio_feature_rows < 0 || audio_feature_rows > EMBED_MAX_AUDIO_TOKENS) {
        set_error(err_buf, err_buf_len, "feature row count out of range");
        return -1;
    }
    if ((image_feature_rows > 0 && !image_features) ||
        (video_feature_rows > 0 && !video_features) ||
        (audio_feature_rows > 0 && !audio_features)) {
        set_error(err_buf, err_buf_len, "feature rows declared without a buffer");
        return -1;
    }

    const OrtApi* ort = session->handle.ort;
    OrtValue* in_vals[5] = {NULL, NULL, NULL, NULL, NULL};
    OrtValue* out_vals[1] = {NULL};

    int64_t dims_seq[2] = {batch_size, seq_len};
    int64_t dims_image[2] = {image_feature_rows, EMBED_FEATURE_DIM};
    int64_t dims_video[2] = {video_feature_rows, EMBED_FEATURE_DIM};
    int64_t dims_audio[2] = {audio_feature_rows, EMBED_FEATURE_DIM};

    struct {
        const void* data;
        size_t byte_len;
        const int64_t* dims;
        size_t num_dims;
        ONNXTensorElementDataType type;
    } inputs[5] = {
        {input_ids, (size_t)(batch_size * seq_len * sizeof(int64_t)), dims_seq, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
        {attention_mask, (size_t)(batch_size * seq_len * sizeof(int64_t)), dims_seq, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
        {image_feature_rows > 0 ? (const void*)image_features : (const void*)empty_features,
         (size_t)image_feature_rows * EMBED_FEATURE_DIM * sizeof(float), dims_image, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT},
        {video_feature_rows > 0 ? (const void*)video_features : (const void*)empty_features,
         (size_t)video_feature_rows * EMBED_FEATURE_DIM * sizeof(float), dims_video, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT},
        {audio_feature_rows > 0 ? (const void*)audio_features : (const void*)empty_features,
         (size_t)audio_feature_rows * EMBED_FEATURE_DIM * sizeof(float), dims_audio, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT},
    };

    for (int i = 0; i < 5; ++i) {
        OrtStatus* status = wrap_tensor(ort, session->handle.mem_info, inputs[i].data,
                                        inputs[i].byte_len, inputs[i].dims,
                                        inputs[i].num_dims, inputs[i].type, &in_vals[i]);
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            for (int j = 0; j < i; ++j) ort->ReleaseValue(in_vals[j]);
            return -(i + 2);
        }
    }

    const char* input_names[5] = {"input_ids", "attention_mask", "image_features",
                                  "video_features", "audio_features"};
    const char* output_names[1] = {"sentence_embedding"};

    OrtStatus* status = ort->Run(
        session->handle.session,
        NULL,
        input_names,
        (const OrtValue* const*)in_vals,
        5,
        output_names,
        1,
        out_vals
    );

    for (int i = 0; i < 5; ++i) {
        ort->ReleaseValue(in_vals[i]);
    }

    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -3;
    }

    int rc = copy_f32_output(ort, out_vals[0], batch_size, EMBED_EMBEDDING_DIM, 0,
                             "embedding", out_embeddings, NULL, err_buf, err_buf_len);
    ort->ReleaseValue(out_vals[0]);
    if (rc == 0 && session->embedding_dim == 0) {
        session->embedding_dim = EMBED_EMBEDDING_DIM;
    }
    return rc;
}

/* ---------------- */
/* EmbeddingGemma vision tower */
/* ---------------- */

struct VisionSession {
    OnnxHandle handle;
};

VisionSession* vision_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
) {
    OnnxHandle* h = onnx_handle_open(onnx_lib_path, model_path, num_threads,
                                     "embed_vision", err_buf, err_buf_len);
    if (!h) return NULL;
    VisionSession* s = (VisionSession*)calloc(1, sizeof(VisionSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating VisionSession");
        onnx_handle_close(h);
        return NULL;
    }
    s->handle = *h;
    free(h);
    return s;
}

void vision_session_destroy(VisionSession* session) {
    if (!session) return;
    onnx_handle_release(&session->handle);
    free(session);
}

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
) {
    if (!session || !session->handle.ort || !session->handle.session ||
        !session->handle.mem_info) {
        set_error(err_buf, err_buf_len, "invalid session");
        return -1;
    }
    /* Every image in the batch is padded to the same patch budget so the
     * tensor stays rectangular; the position ids carry -1 on padding. */
    if (batch_size <= 0 || batch_size > 64 || num_patches != EMBED_IMAGE_PATCHES ||
        !pixel_values || !position_ids || !out_features || !out_rows) {
        set_error(err_buf, err_buf_len, "invalid or oversized vision buffers");
        return -1;
    }

    const OrtApi* ort = session->handle.ort;
    OrtValue* in_vals[2] = {NULL, NULL};
    OrtValue* out_vals[1] = {NULL};

    int64_t dims_pixels[3] = {batch_size, num_patches, EMBED_IMAGE_PATCH_PIXELS};
    int64_t dims_positions[3] = {batch_size, num_patches, 2};

    struct {
        const void* data;
        size_t byte_len;
        const int64_t* dims;
        size_t num_dims;
        ONNXTensorElementDataType type;
    } inputs[2] = {
        {pixel_values, (size_t)(batch_size * num_patches * EMBED_IMAGE_PATCH_PIXELS * sizeof(float)), dims_pixels, 3, ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT},
        {position_ids, (size_t)(batch_size * num_patches * 2 * sizeof(int64_t)), dims_positions, 3, ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64},
    };

    for (int i = 0; i < 2; ++i) {
        OrtStatus* status = wrap_tensor(ort, session->handle.mem_info, inputs[i].data,
                                        inputs[i].byte_len, inputs[i].dims,
                                        inputs[i].num_dims, inputs[i].type, &in_vals[i]);
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            for (int j = 0; j < i; ++j) ort->ReleaseValue(in_vals[j]);
            return -(i + 2);
        }
    }

    const char* input_names[2] = {"pixel_values", "pixel_position_ids"};
    const char* output_names[1] = {"image_features"};

    OrtStatus* status = ort->Run(
        session->handle.session,
        NULL,
        input_names,
        (const OrtValue* const*)in_vals,
        2,
        output_names,
        1,
        out_vals
    );

    for (int i = 0; i < 2; ++i) {
        ort->ReleaseValue(in_vals[i]);
    }

    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -3;
    }

    /* Row count is dynamic: pooling collapses the patch grid, so it depends on
     * the resized image shape. */
    int rc = copy_f32_output(ort, out_vals[0], 0, EMBED_FEATURE_DIM, 1, "image_features",
                             out_features, out_rows, err_buf, err_buf_len);
    ort->ReleaseValue(out_vals[0]);
    if (rc == 0 && (*out_rows < 0 || *out_rows > EMBED_MAX_IMAGE_TOKENS)) {
        set_error(err_buf, err_buf_len, "image feature row count out of range");
        return -12;
    }
    return rc;
}

/* ---------------- */
/* EmbeddingGemma audio tower */
/* ---------------- */

struct AudioSession {
    OnnxHandle handle;
};

AudioSession* audio_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
    char* err_buf,
    size_t err_buf_len
) {
    OnnxHandle* h = onnx_handle_open(onnx_lib_path, model_path, num_threads,
                                     "embed_audio", err_buf, err_buf_len);
    if (!h) return NULL;
    AudioSession* s = (AudioSession*)calloc(1, sizeof(AudioSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating AudioSession");
        onnx_handle_close(h);
        return NULL;
    }
    s->handle = *h;
    free(h);
    return s;
}

void audio_session_destroy(AudioSession* session) {
    if (!session) return;
    onnx_handle_release(&session->handle);
    free(session);
}

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
) {
    if (!session || !session->handle.ort || !session->handle.session ||
        !session->handle.mem_info) {
        set_error(err_buf, err_buf_len, "invalid session");
        return -1;
    }
    /* 480k samples at 16 kHz with a 160-sample hop caps the frame count. */
    if (batch_size <= 0 || batch_size > 64 || seq_len <= 0 || seq_len > 4096 ||
        !input_features || !input_features_mask || !out_features || !out_rows) {
        set_error(err_buf, err_buf_len, "invalid or oversized audio buffers");
        return -1;
    }

    const OrtApi* ort = session->handle.ort;
    OrtValue* in_vals[2] = {NULL, NULL};
    OrtValue* out_vals[1] = {NULL};

    int64_t dims_features[3] = {batch_size, seq_len, EMBED_AUDIO_FEATURE_SIZE};
    int64_t dims_mask[2] = {batch_size, seq_len};

    struct {
        const void* data;
        size_t byte_len;
        const int64_t* dims;
        size_t num_dims;
        ONNXTensorElementDataType type;
    } inputs[2] = {
        {input_features, (size_t)(batch_size * seq_len * EMBED_AUDIO_FEATURE_SIZE * sizeof(float)), dims_features, 3, ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT},
        {input_features_mask, (size_t)(batch_size * seq_len * sizeof(uint8_t)), dims_mask, 2, ONNX_TENSOR_ELEMENT_DATA_TYPE_BOOL},
    };

    for (int i = 0; i < 2; ++i) {
        OrtStatus* status = wrap_tensor(ort, session->handle.mem_info, inputs[i].data,
                                        inputs[i].byte_len, inputs[i].dims,
                                        inputs[i].num_dims, inputs[i].type, &in_vals[i]);
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            for (int j = 0; j < i; ++j) ort->ReleaseValue(in_vals[j]);
            return -(i + 2);
        }
    }

    const char* input_names[2] = {"input_features", "input_features_mask"};
    const char* output_names[1] = {"audio_features"};

    OrtStatus* status = ort->Run(
        session->handle.session,
        NULL,
        input_names,
        (const OrtValue* const*)in_vals,
        2,
        output_names,
        1,
        out_vals
    );

    for (int i = 0; i < 2; ++i) {
        ort->ReleaseValue(in_vals[i]);
    }

    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -3;
    }

    /* Row count is dynamic: the subsampling stack reduces the frame count by
     * the audio duration. */
    int rc = copy_f32_output(ort, out_vals[0], 0, EMBED_FEATURE_DIM, 1, "audio_features",
                             out_features, out_rows, err_buf, err_buf_len);
    ort->ReleaseValue(out_vals[0]);
    if (rc == 0 && (*out_rows < 0 || *out_rows > EMBED_MAX_AUDIO_TOKENS)) {
        set_error(err_buf, err_buf_len, "audio feature row count out of range");
        return -12;
    }
    return rc;
}
