// Control-thread-only descriptor handoff for immutable Darwin shared memory.
#ifndef SOTF_SHARED_MEMORY_DESCRIPTOR_H
#define SOTF_SHARED_MEMORY_DESCRIPTOR_H

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <unistd.h>

#define SOTF_RENDEZVOUS_LENGTH 32
#define SOTF_MAX_SHARED_MEMORY_LENGTH (64ULL * 1024ULL * 1024ULL)

// Accept only an already allocated Darwin POSIX shm object. Its fstat has no
// vnode type bits, and its extent rejects even a same-size ftruncate. Ordinary
// files cannot acquire this type by changing their permissions.
static inline int sotf_validate_memory_descriptor(int fd, uid_t owner, uint64_t length) {
    struct stat info;
    if (length < 192 || length > SOTF_MAX_SHARED_MEMORY_LENGTH || fstat(fd, &info) != 0)
        return -1;
    if ((info.st_mode & S_IFMT) != 0 || info.st_uid != owner || info.st_size < 0 ||
        (uint64_t)info.st_size < length ||
        (uint64_t)info.st_size > SOTF_MAX_SHARED_MEMORY_LENGTH + 65536) {
        errno = EACCES;
        return -1;
    }
    if (ftruncate(fd, info.st_size) != -1 || errno != EINVAL) {
        errno = EINVAL;
        return -1;
    }
    return 0;
}

// Returns one owned FD, or -1. Caller closes a successful result. All ancillary
// FDs are closed on every failure path; socket/connect/read waits are bounded.
// `record` must point to exactly record_length readable bytes during this call.
static inline int sotf_receive_memory_descriptor(
    const char *socket_path, uid_t owner, const uint8_t *record, size_t record_length
) {
    if (record_length != SOTF_RENDEZVOUS_LENGTH ||
        memcmp(record, "SOTFMM08", 8) != 0) {
        errno = EINVAL;
        return -1;
    }
    uint64_t length = 0;
    for (int index = 0; index < 8; ++index)
        length |= ((uint64_t)record[8 + index]) << (8 * index);
    if (length < 192 || length > SOTF_MAX_SHARED_MEMORY_LENGTH) {
        errno = EINVAL;
        return -1;
    }

    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    const size_t path_length = strlen(socket_path);
    if (path_length >= sizeof(address.sun_path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, socket_path, path_length + 1);
    const socklen_t address_length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + path_length + 1);
    address.sun_len = (uint8_t)address_length;

    int socket_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (socket_fd < 0) return -1;
    int received_fd = -1;
    int failure = 0;
    int one = 1;
    struct timeval timeout = { .tv_sec = 0, .tv_usec = 250000 };
    if (fcntl(socket_fd, F_SETFD, FD_CLOEXEC) != 0 ||
        fcntl(socket_fd, F_SETFL, O_NONBLOCK) != 0 ||
        setsockopt(socket_fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one)) != 0 ||
        setsockopt(socket_fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0 ||
        setsockopt(socket_fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0) {
        failure = errno;
        goto finish;
    }
    if (connect(socket_fd, (const struct sockaddr *)&address, address_length) != 0) {
        if (errno != EINPROGRESS && errno != EAGAIN) {
            failure = errno;
            goto finish;
        }
        struct pollfd pending = { .fd = socket_fd, .events = POLLOUT, .revents = 0 };
        int ready = poll(&pending, 1, 250);
        if (ready <= 0) {
            failure = ready == 0 ? ETIMEDOUT : errno;
            goto finish;
        }
        int socket_error = 0;
        socklen_t error_length = sizeof(socket_error);
        if (getsockopt(socket_fd, SOL_SOCKET, SO_ERROR, &socket_error, &error_length) != 0 || socket_error != 0) {
            failure = socket_error != 0 ? socket_error : errno;
            goto finish;
        }
    }
    uid_t peer_uid;
    gid_t peer_gid;
    if (getpeereid(socket_fd, &peer_uid, &peer_gid) != 0 || peer_uid != owner) {
        failure = EACCES;
        goto finish;
    }
    if (fcntl(socket_fd, F_SETFL, 0) != 0) {
        failure = errno;
        goto finish;
    }
    // One small request frame; a short write is a failed handshake, never an
    // invitation to retry indefinitely on a control thread.
    if (send(socket_fd, record, record_length, 0) != (ssize_t)record_length) {
        failure = errno != 0 ? errno : EIO;
        goto finish;
    }

    uint8_t response[SOTF_RENDEZVOUS_LENGTH];
    struct iovec body = { .iov_base = response, .iov_len = sizeof(response) };
    union {
        struct cmsghdr alignment;
        unsigned char bytes[CMSG_SPACE(sizeof(int) * 4)];
    } ancillary;
    memset(&ancillary, 0, sizeof(ancillary));
    struct msghdr message;
    memset(&message, 0, sizeof(message));
    message.msg_iov = &body;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.bytes;
    message.msg_controllen = sizeof(ancillary.bytes);
    const ssize_t count = recvmsg(socket_fd, &message, MSG_WAITALL);
    int malformed = count != (ssize_t)sizeof(response) || (message.msg_flags & MSG_CTRUNC) != 0;
    if (count == (ssize_t)sizeof(response) && memcmp(response, record, sizeof(response)) != 0)
        malformed = 1;
    int descriptor_count = 0;
    if (count >= 0) {
        size_t control_length = message.msg_controllen;
        if (control_length > sizeof(ancillary.bytes)) {
            malformed = 1;
            control_length = sizeof(ancillary.bytes);
        }
        const uintptr_t control_begin = (uintptr_t)ancillary.bytes;
        const uintptr_t control_end = control_begin + control_length;
        for (struct cmsghdr *item = CMSG_FIRSTHDR(&message); item; item = CMSG_NXTHDR(&message, item)) {
            const uintptr_t position = (uintptr_t)item;
            if (position < control_begin || position > control_end ||
                control_end - position < sizeof(*item) ||
                item->cmsg_len > control_end - position) {
                malformed = 1;
                break;
            }
            if (item->cmsg_level != SOL_SOCKET || item->cmsg_type != SCM_RIGHTS || item->cmsg_len < CMSG_LEN(0)) {
                malformed = 1;
                continue;
            }
            size_t bytes = item->cmsg_len - CMSG_LEN(0);
            if (bytes % sizeof(int) != 0) malformed = 1;
            for (size_t offset = 0; offset + sizeof(int) <= bytes; offset += sizeof(int)) {
                int fd;
                memcpy(&fd, CMSG_DATA(item) + offset, sizeof(fd));
                ++descriptor_count;
                if (received_fd < 0) received_fd = fd;
                else close(fd);
            }
        }
    }
    if (malformed || descriptor_count != 1) {
        failure = EPROTO;
        goto finish;
    }
    if (fcntl(received_fd, F_SETFD, FD_CLOEXEC) != 0 ||
        sotf_validate_memory_descriptor(received_fd, owner, length) != 0) {
        failure = errno;
        goto finish;
    }

finish:
    close(socket_fd);
    if (failure != 0) {
        if (received_fd >= 0) close(received_fd);
        errno = failure;
        return -1;
    }
    return received_fd;
}

#endif
