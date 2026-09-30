#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>

#ifdef __linux__
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/landlock.h>
#include <linux/seccomp.h>
#include <linux/securebits.h>
#include <sched.h>
#include <stddef.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <signal.h>
#include <unistd.h>

static int allow_read(int ruleset, const char* path, int directory, int optional) {
    int fd = open(path, O_PATH | O_CLOEXEC);
    if (fd < 0) return optional && errno == ENOENT ? 0 : -1;
    struct landlock_path_beneath_attr rule = {
        .allowed_access = LANDLOCK_ACCESS_FS_READ_FILE |
            (directory ? LANDLOCK_ACCESS_FS_READ_DIR : 0),
        .parent_fd = fd,
    };
    int rc = syscall(SYS_landlock_add_rule, ruleset, LANDLOCK_RULE_PATH_BENEATH, &rule, 0);
    close(fd);
    return rc;
}

static int restrict_files(const char* model, const char* tokenizer, const char* config) {
    /* ABI 3 handles truncate as well as creation/removal/writes. Fail closed on
     * older kernels: a process boundary alone is not a security sandbox. */
    int abi = syscall(SYS_landlock_create_ruleset, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION);
    if (abi < 3) return -1;
    struct landlock_ruleset_attr rules = {
        .handled_access_fs = (LANDLOCK_ACCESS_FS_REFER << 1) - 1,
    };
    /* TRUNCATE was added in ABI 3 (bit 14). Use its numeric value for older headers. */
    rules.handled_access_fs |= (1ULL << 14);
    int fd = syscall(SYS_landlock_create_ruleset, &rules, sizeof(rules), 0);
    if (fd < 0) return -1;
    int rc = allow_read(fd, model, 0, 0) || allow_read(fd, tokenizer, 0, 0) ||
        allow_read(fd, config, 0, 1) ||
        allow_read(fd, "/usr/lib", 1, 1) || allow_read(fd, "/usr/local/lib", 1, 1) ||
        allow_read(fd, "/lib", 1, 1) || allow_read(fd, "/lib64", 1, 1) ||
        allow_read(fd, "/etc/ld.so.cache", 0, 1) ||
        allow_read(fd, "/sys/devices/system/cpu", 1, 1) ||
        allow_read(fd, "/proc/cpuinfo", 0, 1) || allow_read(fd, "/proc/meminfo", 0, 1);
    const char* lib = getenv("OPENPROXY_ONNX_LIB");
    if (!rc && lib) rc = allow_read(fd, lib, 0, 0);
    if (!rc) rc = syscall(SYS_landlock_restrict_self, fd, 0);
    close(fd);
    return rc;
}

#define DENY_SYSCALL(nr) \
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1), \
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM)

static int restrict_syscalls(void) {
#if defined(__x86_64__)
    const uint32_t arch = AUDIT_ARCH_X86_64;
#elif defined(__aarch64__)
    const uint32_t arch = AUDIT_ARCH_AARCH64;
#else
    return -1;
#endif
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, arch, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
#if defined(__x86_64__)
        /* Reject x32 syscall aliases that would bypass the native-number checks. */
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, 0x40000000, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
#endif
        DENY_SYSCALL(SYS_socket), DENY_SYSCALL(SYS_socketpair),
        DENY_SYSCALL(SYS_connect), DENY_SYSCALL(SYS_bind),
        DENY_SYSCALL(SYS_ptrace), DENY_SYSCALL(SYS_process_vm_readv),
        DENY_SYSCALL(SYS_process_vm_writev), DENY_SYSCALL(SYS_kill),
        DENY_SYSCALL(SYS_tkill), DENY_SYSCALL(SYS_pidfd_open),
        DENY_SYSCALL(SYS_rt_sigqueueinfo), DENY_SYSCALL(SYS_rt_tgsigqueueinfo),
        DENY_SYSCALL(SYS_process_madvise),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_tgkill, 0, 4),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (uint32_t)getpid(), 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        DENY_SYSCALL(SYS_pidfd_getfd), DENY_SYSCALL(SYS_pidfd_send_signal),
        DENY_SYSCALL(SYS_execve), DENY_SYSCALL(SYS_execveat),
        DENY_SYSCALL(SYS_mount), DENY_SYSCALL(SYS_umount2),
        DENY_SYSCALL(SYS_unshare), DENY_SYSCALL(SYS_setns),
        DENY_SYSCALL(SYS_bpf), DENY_SYSCALL(SYS_perf_event_open),
        DENY_SYSCALL(SYS_io_uring_setup), DENY_SYSCALL(SYS_userfaultfd),
        DENY_SYSCALL(SYS_keyctl), DENY_SYSCALL(SYS_add_key),
        DENY_SYSCALL(SYS_request_key), DENY_SYSCALL(SYS_capset),
        DENY_SYSCALL(SYS_setuid), DENY_SYSCALL(SYS_setgid),
        DENY_SYSCALL(SYS_setreuid), DENY_SYSCALL(SYS_setregid),
        DENY_SYSCALL(SYS_setresuid), DENY_SYSCALL(SYS_setresgid),
        DENY_SYSCALL(SYS_setfsuid), DENY_SYSCALL(SYS_setfsgid),
        DENY_SYSCALL(SYS_setgroups), DENY_SYSCALL(SYS_reboot),
        DENY_SYSCALL(SYS_init_module), DENY_SYSCALL(SYS_finit_module),
        DENY_SYSCALL(SYS_delete_module), DENY_SYSCALL(SYS_kexec_load),
        DENY_SYSCALL(SYS_swapon), DENY_SYSCALL(SYS_swapoff),
        DENY_SYSCALL(SYS_syslog), DENY_SYSCALL(SYS_acct),
#ifdef SYS_fork
        DENY_SYSCALL(SYS_fork), DENY_SYSCALL(SYS_vfork),
#endif
        /* libc falls back to clone when clone3 returns ENOSYS. */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone3, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 0, 4),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, CLONE_THREAD, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = { .len = sizeof(filter) / sizeof(filter[0]), .filter = filter };
    return prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program);
}

int laya_worker_sandbox(const char* model, const char* tokenizer, const char* config) {
    /* Defense in depth: discard non-CLOEXEC descriptors from any gateway library. */
    if (syscall(SYS_close_range, 3U, UINT_MAX, 0U)) return -9;
    struct rlimit no_core = {0, 0};
    struct rlimit memory = {4ULL * 1024 * 1024 * 1024, 4ULL * 1024 * 1024 * 1024};
    struct rlimit files = {64, 64};
    if (setrlimit(RLIMIT_CORE, &no_core) || setrlimit(RLIMIT_AS, &memory) ||
        setrlimit(RLIMIT_NOFILE, &files)) return -1;
    if (prctl(PR_SET_DUMPABLE, 0) || prctl(PR_SET_PDEATHSIG, SIGKILL) ||
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return -2;
    if (getppid() == 1) return -3;
    if (geteuid() == 0 && prctl(PR_SET_SECUREBITS,
            SECBIT_NOROOT | SECBIT_NOROOT_LOCKED |
            SECBIT_NO_SETUID_FIXUP | SECBIT_NO_SETUID_FIXUP_LOCKED)) return -7;
    struct __user_cap_header_struct header = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct capabilities[2] = {{0}, {0}};
    if (syscall(SYS_capset, &header, capabilities)) return -8;
    if (restrict_files(model, tokenizer, config)) return -4;
    if (restrict_syscalls()) return -5;
    return 0;
}
#else
/* No silent fallback to unconfined native execution on unsupported platforms. */
int laya_worker_sandbox(const char* model, const char* tokenizer, const char* config) {
    (void)model; (void)tokenizer; (void)config;
    return -6;
}
#endif
