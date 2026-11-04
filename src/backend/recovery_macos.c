// Native wake and network notifications for the dashboard's process lifetime.
// Keep platform types in C so SDK declarations check callback signatures.
#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOMessage.h>
#include <IOKit/pwr_mgt/IOPMLib.h>
#include <SystemConfiguration/SystemConfiguration.h>

typedef void (*recovery_callback)(int);
struct monitor {
    io_connect_t power;
    recovery_callback signal;
};

static void power_changed(void *context, io_service_t service,
                          natural_t message, void *argument) {
    (void)service;
    struct monitor *monitor = context;
    switch (message) {
    case kIOMessageCanSystemSleep:
    case kIOMessageSystemWillSleep:
        IOAllowPowerChange(monitor->power, (long)argument);
        break;
    case kIOMessageSystemHasPoweredOn:
        monitor->signal(1);
        break;
    default:
        break;
    }
}

static void network_changed(SCDynamicStoreRef store, CFArrayRef keys, void *context) {
    (void)store;
    (void)keys;
    struct monitor *monitor = context;
    monitor->signal(2);
}

int cm_watch_recovery(recovery_callback callback) {
    struct monitor monitor = { .power = 0, .signal = callback };
    IONotificationPortRef port = NULL;
    io_object_t notifier = 0;
    monitor.power = IORegisterForSystemPower(&monitor, &port, power_changed, &notifier);
    CFRunLoopRef loop = CFRunLoopGetCurrent();
    if (monitor.power)
        CFRunLoopAddSource(loop, IONotificationPortGetRunLoopSource(port), kCFRunLoopDefaultMode);

    SCDynamicStoreContext context = { 0, &monitor, NULL, NULL, NULL };
    SCDynamicStoreRef store = SCDynamicStoreCreate(NULL, CFSTR("captain-miao recovery"),
                                                 network_changed, &context);
    CFRunLoopSourceRef source = NULL;
    if (store) {
        const void *patterns[] = {
            CFSTR("State:/Network/Global/IPv4"),
            CFSTR("State:/Network/Global/IPv6"),
            CFSTR("State:/Network/Global/DNS"),
            CFSTR("State:/Network/Interface/.*/Link"),
            CFSTR("State:/Network/Interface/.*/IPv4"),
            CFSTR("State:/Network/Interface/.*/IPv6"),
        };
        CFArrayRef keys = CFArrayCreate(NULL, patterns, sizeof(patterns) / sizeof(patterns[0]),
                                        &kCFTypeArrayCallBacks);
        if (SCDynamicStoreSetNotificationKeys(store, NULL, keys))
            source = SCDynamicStoreCreateRunLoopSource(NULL, store, 0);
        CFRelease(keys);
        if (source)
            CFRunLoopAddSource(loop, source, kCFRunLoopDefaultMode);
    }

    // Fail visibly if either registration failed; normal SSH recovery remains
    // available. Both registrations are required for wake followed by Wi-Fi.
    int status = (monitor.power && source) ? 0 : 1;
    if (!status)
        CFRunLoopRun();

    if (source) {
        CFRunLoopRemoveSource(loop, source, kCFRunLoopDefaultMode);
        CFRelease(source);
    }
    if (store)
        CFRelease(store);
    if (monitor.power) {
        CFRunLoopRemoveSource(loop, IONotificationPortGetRunLoopSource(port), kCFRunLoopDefaultMode);
        IODeregisterForSystemPower(&notifier);
        IOServiceClose(monitor.power);
    }
    if (port)
        IONotificationPortDestroy(port);
    return status;
}
