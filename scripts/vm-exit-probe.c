#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <libkrun.h>

static void check(const char *name, int value) {
    if (value < 0) { fprintf(stderr, "%s: %d\n", name, value); exit(1); }
}

int main(int argc, char **argv) {
    if (argc != 3 && argc != 6) return 2;
    int ctx = krun_create_ctx();
    check("create", ctx);
    check("vsock", krun_disable_implicit_vsock(ctx));
    check("config", krun_set_vm_config(ctx, 2, 1024));
    check("disk", krun_add_disk3(ctx, "root", argv[1], 0, false, false, 1));
    if (argc == 6) check("journal", krun_add_disk3(ctx, "journal", argv[3], 0, false, false, 2));
    check("root", krun_set_root_disk_remount(ctx, "/dev/vda", "ext4", NULL));
    check("console", krun_set_console_output(ctx, argv[2]));
    check("workdir", krun_set_workdir(ctx, "/"));
    const char *args[] = {argc == 6 ? argv[5] : "DECK_EXIT_PROBE", NULL};
    const char *env[] = {"PATH=/usr/sbin:/usr/bin:/sbin:/bin", NULL};
    check("exec", krun_set_exec(ctx, argc == 6 ? argv[4] : "/bin/echo", args, env));
    fprintf(stderr, "starting guest\n");
    int result = krun_start_enter(ctx);
    fprintf(stderr, "krun_start_enter returned %d\n", result);
    return result < 0 ? 1 : 0;
}
