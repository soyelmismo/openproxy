#define _GNU_SOURCE
#include "laya_bridge.h"
#include "onnxruntime_c_api.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#define DL_OPEN(path) (void*)LoadLibraryA(path)
#define DL_SYM(handle, sym) (void*)GetProcAddress((HMODULE)(handle), (sym))
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

struct LayaSession {
    void* dl_handle;
    const OrtApi* ort;
    OrtEnv* env;
    OrtSessionOptions* opts;
    OrtSession* session;
    OrtMemoryInfo* mem_info;
};

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

LayaSession* laya_session_create(
    const char* onnx_lib_path,
    const char* model_path,
    int num_threads,
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
            set_error(err_buf, err_buf_len,
                      "OPENPROXY_ONNX_LIB must be an absolute path");
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

    // Use API version 20 (compatible with ORT 1.20+)
    const OrtApi* ort = base->GetApi(20);
    if (!ort) {
        set_error(err_buf, err_buf_len, "OrtApi version 20 not supported by runtime");
        DL_CLOSE(handle);
        return NULL;
    }

    LayaSession* s = (LayaSession*)calloc(1, sizeof(LayaSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating LayaSession");
        DL_CLOSE(handle);
        return NULL;
    }
    s->dl_handle = handle;
    s->ort = ort;

    OrtStatus* status = ort->CreateEnv(ORT_LOGGING_LEVEL_WARNING, "laya_engine", &s->env);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        laya_session_destroy(s);
        return NULL;
    }

    status = ort->CreateSessionOptions(&s->opts);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        laya_session_destroy(s);
        return NULL;
    }

    if (num_threads > 0) {
        ort->SetIntraOpNumThreads(s->opts, num_threads);
    }
    ort->SetSessionGraphOptimizationLevel(s->opts, ORT_ENABLE_ALL);

#ifdef _WIN32
    wchar_t w_model_path[1024];
    MultiByteToWideChar(CP_UTF8, 0, model_path, -1, w_model_path, 1024);
    status = ort->CreateSession(s->env, w_model_path, s->opts, &s->session);
#else
    status = ort->CreateSession(s->env, model_path, s->opts, &s->session);
#endif
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        laya_session_destroy(s);
        return NULL;
    }

    status = ort->CreateCpuMemoryInfo(OrtArenaAllocator, OrtMemTypeDefault, &s->mem_info);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        laya_session_destroy(s);
        return NULL;
    }

    return s;
}

void laya_session_destroy(LayaSession* session) {
    if (!session) return;
    if (session->ort) {
        if (session->mem_info) {
            session->ort->ReleaseMemoryInfo(session->mem_info);
            session->mem_info = NULL;
        }
        if (session->session) {
            session->ort->ReleaseSession(session->session);
            session->session = NULL;
        }
        if (session->opts) {
            session->ort->ReleaseSessionOptions(session->opts);
            session->opts = NULL;
        }
        if (session->env) {
            session->ort->ReleaseEnv(session->env);
            session->env = NULL;
        }
    }
    if (session->dl_handle) {
        DL_CLOSE(session->dl_handle);
        session->dl_handle = NULL;
    }
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
    if (!session || !session->ort || !session->session || !session->mem_info) {
        set_error(err_buf, err_buf_len, "invalid session");
        return -1;
    }

    if (batch_size <= 0 || batch_size > 64 || seq_len <= 0 || seq_len > 8192 ||
        max_markers <= 0 || max_markers > 128 || !input_ids || !attention_mask ||
        !marker_pos || !marker_mask || !qtype || !out_logits) {
        set_error(err_buf, err_buf_len, "invalid or oversized inference buffers");
        return -1;
    }

    const OrtApi* ort = session->ort;
    OrtStatus* status = NULL;
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
        status = ort->CreateTensorWithDataAsOrtValue(
            session->mem_info,
            (void*)inputs[i].data,
            inputs[i].byte_len,
            inputs[i].dims,
            inputs[i].num_dims,
            inputs[i].type,
            &in_vals[i]
        );
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            for (int j = 0; j < i; ++j) ort->ReleaseValue(in_vals[j]);
            return -(i + 2);
        }
    }

    const char* input_names[5] = {"input_ids", "attention_mask", "marker_pos", "marker_mask", "qtype"};
    const char* output_names[1] = {"logits"};

    status = ort->Run(
        session->session,
        NULL,
        input_names,
        (const OrtValue* const*)in_vals,
        5,
        output_names,
        1,
        out_vals
    );

    // Free inputs immediately
    for (int i = 0; i < 5; ++i) {
        ort->ReleaseValue(in_vals[i]);
    }

    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        return -7;
    }

    // Extract logits
    float* logits_ptr = NULL;
    status = ort->GetTensorMutableData(out_vals[0], (void**)&logits_ptr);
    if (status) {
        set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
        ort->ReleaseStatus(status);
        ort->ReleaseValue(out_vals[0]);
        return -8;
    }

    // Security (OP-12): validate the output tensor's shape BEFORE the copy.
    // The element count used to be derived solely from the batch_size and
    // max_markers arguments passed by Rust; a model (or a replaced model
    // file) declaring a smaller "logits" output made the memcpy below read
    // past the end of the tensor buffer — a heap over-read in the gateway
    // process.
    {
        OrtTensorTypeAndShapeInfo* shape_info = NULL;
        size_t element_count = 0;
        size_t rank = 0;
        int64_t dimensions[2] = {0, 0};
        enum ONNXTensorElementDataType element_type =
            ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;

        status = ort->GetTensorTypeAndShape(out_vals[0], &shape_info);
        if (!status) {
            status = ort->GetTensorElementType(shape_info, &element_type);
        }
        if (!status) {
            status = ort->GetTensorShapeElementCount(shape_info, &element_count);
        }
        if (!status) {
            status = ort->GetDimensionsCount(shape_info, &rank);
        }
        if (!status && rank == 2) {
            status = ort->GetDimensions(shape_info, dimensions, 2);
        }
        if (shape_info) {
            ort->ReleaseTensorTypeAndShapeInfo(shape_info);
        }
        if (status) {
            set_error(err_buf, err_buf_len, ort->GetErrorMessage(status));
            ort->ReleaseStatus(status);
            ort->ReleaseValue(out_vals[0]);
            return -9;
        }
        if (element_type != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT ||
            rank != 2 || dimensions[0] != batch_size || dimensions[1] != max_markers ||
            element_count != (size_t)batch_size * (size_t)max_markers) {
            char msg[160];
            snprintf(msg, sizeof(msg),
                     "logits tensor mismatch: type=%d elements=%zu, expected %zu FLOAT",
                     (int)element_type, element_count, (size_t)batch_size * (size_t)max_markers);
            set_error(err_buf, err_buf_len, msg);
            ort->ReleaseValue(out_vals[0]);
            return -10;
        }
    }

    memcpy(out_logits, logits_ptr, (size_t)(batch_size * max_markers * sizeof(float)));
    ort->ReleaseValue(out_vals[0]);

    return 0;
}
