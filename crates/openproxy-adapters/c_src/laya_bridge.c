#define _GNU_SOURCE
#include "laya_bridge.h"
#include "onnxruntime_c_api.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <unistd.h>

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
    "libonnxruntime.so",
    "libonnxruntime.so.1",
    "/opt/homebrew/lib/libonnxruntime.dylib",
    "/usr/local/lib/libonnxruntime.dylib",
    "libonnxruntime.dylib",
    "/root/code/laya-playground/.venv/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.1.30.0",
    NULL
};

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
        handle = dlopen(onnx_lib_path, RTLD_NOW | RTLD_GLOBAL);
    }
    if (!handle && env_path && env_path[0] != '\0') {
        handle = dlopen(env_path, RTLD_NOW | RTLD_GLOBAL);
    }
    if (!handle) {
        for (int i = 0; candidate_paths[i] != NULL; ++i) {
            handle = dlopen(candidate_paths[i], RTLD_NOW | RTLD_GLOBAL);
            if (handle) break;
        }
    }

    if (!handle) {
        set_error(err_buf, err_buf_len, dlerror() ? dlerror() : "failed to load libonnxruntime.so");
        return NULL;
    }

    OrtApiBase* (*get_api_base)(void) = (OrtApiBase* (*)(void))dlsym(handle, "OrtGetApiBase");
    if (!get_api_base) {
        set_error(err_buf, err_buf_len, "dlsym OrtGetApiBase failed");
        dlclose(handle);
        return NULL;
    }

    const OrtApiBase* base = get_api_base();
    if (!base) {
        set_error(err_buf, err_buf_len, "OrtGetApiBase returned NULL");
        dlclose(handle);
        return NULL;
    }

    // Use API version 20 (compatible with ORT 1.20+)
    const OrtApi* ort = base->GetApi(20);
    if (!ort) {
        set_error(err_buf, err_buf_len, "OrtApi version 20 not supported by runtime");
        dlclose(handle);
        return NULL;
    }

    LayaSession* s = (LayaSession*)calloc(1, sizeof(LayaSession));
    if (!s) {
        set_error(err_buf, err_buf_len, "out of memory allocating LayaSession");
        dlclose(handle);
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

    status = ort->CreateSession(s->env, model_path, s->opts, &s->session);
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
        dlclose(session->dl_handle);
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

    memcpy(out_logits, logits_ptr, (size_t)(batch_size * max_markers * sizeof(float)));
    ort->ReleaseValue(out_vals[0]);

    return 0;
}
