#import <DiskArbitration/DiskArbitration.h>
#import <Foundation/Foundation.h>
#import <IOKit/IOKitLib.h>
#import <IOKit/storage/IOMedia.h>
#include <sys/mount.h>

typedef struct {
    uint64_t media_id;
    uint64_t whole_id;
    int32_t placement;
    bool can_eject;
    char name[1024];
    char protocol[128];
} VolumeInfo;

enum { Unknown, Internal, External, DiskImage };

static uint64_t media_id(DADiskRef disk) {
    if (!disk) return 0;
    io_service_t media = DADiskCopyIOMedia(disk);
    if (!media) return 0;
    uint64_t id = 0;
    kern_return_t status = IORegistryEntryGetRegistryEntryID(media, &id);
    IOObjectRelease(media);
    return status == KERN_SUCCESS ? id : 0;
}

static int32_t placement(NSDictionary *info) {
    NSNumber *internal = info[(__bridge NSString *)kDADiskDescriptionDeviceInternalKey];
    if (internal) return internal.boolValue ? Internal : External;
    NSString *protocol = info[(__bridge NSString *)kDADiskDescriptionDeviceProtocolKey];
    NSNumber *ejectable = info[(__bridge NSString *)kDADiskDescriptionMediaEjectableKey];
    if ([protocol isEqualToString:@"Virtual Interface"] && ejectable.boolValue) return DiskImage;
    return Unknown;
}

static NSDictionary *disk_info(DADiskRef disk) {
    return disk ? CFBridgingRelease(DADiskCopyDescription(disk)) : nil;
}

// Disk Arbitration can return the synthesized APFS container as the whole
// disk. The outer whole IOMedia owns the actual external device or image.
static DADiskRef copy_storage_disk(DASessionRef session, DADiskRef disk) {
    DADiskRef whole = disk ? DADiskCopyWholeDisk(disk) : NULL;
    if (!whole) return NULL;
    io_registry_entry_t entry = DADiskCopyIOMedia(whole);
    CFRelease(whole);
    io_service_t storage = IO_OBJECT_NULL;
    bool ambiguous = false;
    while (entry) {
        if (IOObjectConformsTo(entry, kIOMediaClass)) {
            NSNumber *is_whole = CFBridgingRelease(IORegistryEntryCreateCFProperty(
                entry, CFSTR(kIOMediaWholeKey), kCFAllocatorDefault, 0));
            if (is_whole.boolValue) {
                if (storage) IOObjectRelease(storage);
                IOObjectRetain(entry);
                storage = entry;
            }
        } else if (storage && IOObjectConformsTo(entry, "IOBlockStorageDriver")) {
            IOObjectRelease(entry);
            break;
        }
        io_iterator_t parents;
        io_registry_entry_t parent = IO_OBJECT_NULL;
        if (IORegistryEntryGetParentIterator(entry, kIOServicePlane, &parents) == KERN_SUCCESS) {
            parent = IOIteratorNext(parents);
            io_registry_entry_t other = IOIteratorNext(parents);
            if (other) {
                IOObjectRelease(other);
                ambiguous = true;
            }
            IOObjectRelease(parents);
        }
        IOObjectRelease(entry);
        if (ambiguous) {
            IOObjectRelease(parent);
            break;
        }
        entry = parent;
    }
    DADiskRef device = storage && !ambiguous
        ? DADiskCreateFromIOMedia(kCFAllocatorDefault, session, storage) : NULL;
    if (storage) IOObjectRelease(storage);
    return device;
}

static bool outside_startup_disk(DASessionRef session, uint64_t storage_id) {
    NSURL *root = [NSURL fileURLWithPath:@"/" isDirectory:YES];
    DADiskRef boot = DADiskCreateFromVolumePath(kCFAllocatorDefault, session, (__bridge CFURLRef)root);
    if (!boot) return false;
    DADiskRef boot_storage = copy_storage_disk(session, boot);
    uint64_t boot_id = media_id(boot_storage);
    if (boot_storage) CFRelease(boot_storage);
    CFRelease(boot);
    return boot_id && boot_id != storage_id;
}

static void copy_string(char *output, size_t size, NSString *value) {
    const char *utf8 = value.UTF8String;
    if (utf8) strlcpy(output, utf8, size);
}

void paw_volume_info(const char *device, VolumeInfo *output) {
    @autoreleasepool {
        DASessionRef session = DASessionCreate(kCFAllocatorDefault);
        if (!session) return;
        DADiskRef disk = DADiskCreateFromBSDName(kCFAllocatorDefault, session, device);
        DADiskRef storage = copy_storage_disk(session, disk);
        NSDictionary *volume_info = disk_info(disk);
        NSDictionary *device_info = disk_info(storage);
        int32_t kind = placement(device_info);
        output->media_id = media_id(disk);
        output->whole_id = media_id(storage);
        output->placement = kind;
        output->can_eject = output->media_id && output->whole_id
            && (kind == External || kind == DiskImage)
            && outside_startup_disk(session, output->whole_id);
        copy_string(output->name, sizeof(output->name),
                    volume_info[(__bridge NSString *)kDADiskDescriptionVolumeNameKey]);
        copy_string(output->protocol, sizeof(output->protocol),
                    device_info[(__bridge NSString *)kDADiskDescriptionDeviceProtocolKey]);
        if (storage) CFRelease(storage);
        if (disk) CFRelease(disk);
        CFRelease(session);
    }
}

static bool same_mount(const char *mount, const char *device, int32_t first, int32_t second) {
    struct statfs stat;
    return strcmp(mount, "/") != 0 && statfs(mount, &stat) == 0
        && (stat.f_flags & (MNT_LOCAL | MNT_ROOTFS | MNT_DONTBROWSE)) == MNT_LOCAL
        && strcmp(stat.f_mntonname, mount) == 0 && strcmp(stat.f_mntfromname, device) == 0
        && stat.f_fsid.val[0] == first && stat.f_fsid.val[1] == second;
}

bool paw_volume_matches(const char *mount, const char *device, int32_t first, int32_t second,
                        uint64_t expected_media, uint64_t expected_whole) {
    @autoreleasepool {
        if (!expected_media || !expected_whole) return false;
        DASessionRef session = DASessionCreate(kCFAllocatorDefault);
        if (!session) return false;
        DADiskRef disk = DADiskCreateFromBSDName(kCFAllocatorDefault, session, device);
        DADiskRef storage = copy_storage_disk(session, disk);
        int32_t kind = placement(disk_info(storage));
        bool matches = (kind == External || kind == DiskImage)
            && outside_startup_disk(session, expected_whole)
            && same_mount(mount, device, first, second)
            && media_id(disk) == expected_media && media_id(storage) == expected_whole;
        if (storage) CFRelease(storage);
        if (disk) CFRelease(disk);
        CFRelease(session);
        return matches;
    }
}

bool paw_volume_image_attached(uint64_t id) {
    // The matching call consumes the dictionary.
    CFMutableDictionaryRef match = IORegistryEntryIDMatching(id);
    if (!match) return true;
    io_service_t media = IOServiceGetMatchingService(MACH_PORT_NULL, match);
    if (!media) return false;
    IOObjectRelease(media);
    return true;
}
