#import <Foundation/Foundation.h>
#include <NetFS/NetFS.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <unistd.h>

int paw_smb_mount(const char *address, const char *path, const char *user,
                  const unsigned char *password, size_t password_len) {
    @autoreleasepool {
        NSURL *server = [NSURL URLWithString:[NSString stringWithUTF8String:address]];
        NSURL *mount = [NSURL fileURLWithFileSystemRepresentation:path isDirectory:YES relativeToURL:nil];
        NSString *username = user ? [NSString stringWithUTF8String:user] : nil;
        NSString *secret = user ? [[NSString alloc] initWithBytes:password length:password_len
                                                         encoding:NSUTF8StringEncoding] : nil;
        if (!server || !mount || (user && (!username || !secret))) {
            return EINVAL;
        }
        NSMutableDictionary *open_options = [@{
            (__bridge NSString *)kNAUIOptionKey: (__bridge NSString *)kNAUIOptionNoUI,
            (__bridge NSString *)kNetFSForceNewSessionKey: @YES,
            (__bridge NSString *)kNetFSAllowLoopbackKey: @YES,
            (__bridge NSString *)kNetFSUseGuestKey: @(!user),
        } mutableCopy];
        NSMutableDictionary *mount_options = [@{
            (__bridge NSString *)kNetFSMountAtMountDirKey: @YES,
            (__bridge NSString *)kNetFSSoftMountKey: @YES,
            (__bridge NSString *)kNetFSMountFlagsKey: @(MNT_DONTBROWSE | MNT_NOSUID | MNT_NODEV),
        } mutableCopy];
        CFArrayRef paths = NULL;
        int status = NetFSMountURLSync((__bridge CFURLRef)server, (__bridge CFURLRef)mount,
            (__bridge CFStringRef)username, (__bridge CFStringRef)secret,
            (__bridge CFMutableDictionaryRef)open_options,
            (__bridge CFMutableDictionaryRef)mount_options, &paths);
        // The caller also checks the live mount table before exposing the path.
        if (paths) CFRelease(paths);
        return status;
    }
}

int paw_smb_mount_id(const char *path, bool exact, int32_t *id) {
    int count = getfsstat(NULL, 0, MNT_NOWAIT);
    if (count < 0) return errno;
    if (count == 0) return ENOENT;
    struct statfs *mounts = calloc((size_t)count, sizeof(*mounts));
    if (!mounts) return ENOMEM;
    int scanned = getfsstat(mounts, (long)((size_t)count * sizeof(*mounts)), MNT_NOWAIT);
    int status = scanned < 0 ? errno : ENOENT;
    size_t longest = 0;
    size_t path_length = strlen(path);
    for (int i = 0; i < scanned && i < count; i++) {
        size_t length = strnlen(mounts[i].f_mntonname, sizeof(mounts[i].f_mntonname));
        if (length < longest || length > path_length || (exact && length != path_length)
            || strncmp(mounts[i].f_mntonname, path, length) != 0
            || (length != path_length && path[length] != '/' && length != 1)) {
            continue;
        }
        longest = length;
        if (strcmp(mounts[i].f_fstypename, "smbfs") != 0
            || (exact && mounts[i].f_owner != getuid())) {
            status = exact ? EXDEV : ENOENT;
            continue;
        }
        id[0] = mounts[i].f_fsid.val[0];
        id[1] = mounts[i].f_fsid.val[1];
        status = 0;
    }
    free(mounts);
    return status;
}

int paw_smb_directory_id(int fd, int32_t *id, bool *is_smb) {
    struct statfs volume;
    if (fstatfs(fd, &volume) != 0) return errno;
    *is_smb = strcmp(volume.f_fstypename, "smbfs") == 0;
    id[0] = volume.f_fsid.val[0];
    id[1] = volume.f_fsid.val[1];
    return 0;
}
