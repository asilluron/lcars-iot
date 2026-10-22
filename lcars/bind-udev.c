#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

/* The touchscreen is on the host, and libinput only sees it through the
   host udev socket. This supervisor will not mount /run/udev, and a bind
   of /proc/1/root/run/udev attaches the empty directory underneath the
   host /run mount. Clone the real directory from systemd's mount
   namespace, then attach that clone in ours. */

#ifndef OPEN_TREE_CLONE
#define OPEN_TREE_CLONE 1
#endif
#ifndef OPEN_TREE_CLOEXEC
#define OPEN_TREE_CLOEXEC 02000000
#endif
#ifndef MOVE_MOUNT_F_EMPTY_PATH
#define MOVE_MOUNT_F_EMPTY_PATH 0x00000004
#endif

static int sys_open_tree(int dfd, const char *path, unsigned int flags) {
    return syscall(__NR_open_tree, dfd, path, flags);
}

static int sys_move_mount(int from_dfd, const char *from, int to_dfd,
                          const char *to, unsigned int flags) {
    return syscall(__NR_move_mount, from_dfd, from, to_dfd, to, flags);
}

int main(void) {
    if (mkdir("/run/udev", 0755) != 0 && errno != EEXIST) {
        fprintf(stderr, "lcars: mkdir udev: %s\n", strerror(errno));
        return 1;
    }

    int self_mnt = open("/proc/self/ns/mnt", O_RDONLY);
    int host_mnt = open("/proc/1/ns/mnt", O_RDONLY);
    int host_root = open("/proc/1/root", O_RDONLY | O_DIRECTORY);
    if (self_mnt < 0 || host_mnt < 0 || host_root < 0) {
        fprintf(stderr, "lcars: open namespaces: %s\n", strerror(errno));
        return 1;
    }
    if (setns(host_mnt, CLONE_NEWNS) != 0) {
        fprintf(stderr, "lcars: enter host mounts: %s\n", strerror(errno));
        return 1;
    }
    int tree = sys_open_tree(host_root, "run/udev", OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC);
    int saved = errno;
    if (setns(self_mnt, CLONE_NEWNS) != 0) {
        fprintf(stderr, "lcars: return to container mounts: %s\n", strerror(errno));
        return 1;
    }
    if (tree < 0) {
        fprintf(stderr, "lcars: open udev tree: %s\n", strerror(saved));
        return 1;
    }
    if (sys_move_mount(tree, "", AT_FDCWD, "/run/udev", MOVE_MOUNT_F_EMPTY_PATH) != 0) {
        fprintf(stderr, "lcars: move udev: %s\n", strerror(errno));
        return 1;
    }
    return 0;
}
