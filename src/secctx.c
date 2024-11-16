#include "security-context-v1-protocol.h"
#include "util.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <wayland-client.h>

static struct wp_security_context_manager_v1 *security_context_manager = NULL;
static struct wp_security_context_v1 *security_context = NULL;
static int listen_fd = -1;
static int close_fd[2] = {-1, -1};

static void registry_handle_global(void *data, struct wl_registry *registry,
		uint32_t name, const char *interface, uint32_t version)
{
	(void)data;
	(void)version;

	if (strcmp(interface, "wp_security_context_manager_v1") == 0) {
		security_context_manager = wl_registry_bind(registry, name,
				&wp_security_context_manager_v1_interface, 1);
	}
}

static void registry_handle_global_remove(
		void *data, struct wl_registry *registry, uint32_t name)
{
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
		.global = registry_handle_global,
		.global_remove = registry_handle_global_remove};

void close_security_context(void)
{
	if (close_fd[1] >= 0) {
		close(close_fd[1]);
		close_fd[1] = -1;
	}
	if (listen_fd >= 0) {
		close(listen_fd);
		listen_fd = -1;
	}
}

int create_security_context(const char *sock_path, const char *engine,
		const char *instance_id, const char *app_id)
{
	struct wl_display *display = NULL;
	struct wl_registry *registry = NULL;
	int res = -1;

	wp_debug("Enabling wayland security context");
	display = wl_display_connect(NULL);
	if (display == NULL) {
		wp_error("Failed to connect to the Wayland compositor");
		goto cleanup;
	}

	registry = wl_display_get_registry(display);
	if (registry == NULL) {
		wp_error("Failed to get Wayland display registry");
		goto cleanup;
	}

	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_dispatch(display);

	if (wl_display_roundtrip(display) == -1) {
		wp_error("Failed to execute display roundtrip");
		goto cleanup;
	}

	if (!security_context_manager) {
		wp_error("Security context is not supported by the Wayland compositor");
		goto cleanup;
	}

	listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (listen_fd < 0) {
		wp_error("Failed to create a Unix socket for security context");
		goto cleanup;
	}

	struct sockaddr_un sockaddr = {0};
	sockaddr.sun_family = AF_UNIX;
	strncpy(sockaddr.sun_path, sock_path, sizeof(sockaddr.sun_path) - 1);
	if (bind(listen_fd, (struct sockaddr *)&sockaddr, sizeof(sockaddr)) !=
			0) {
		wp_error("Failed to bind the Unix socket for the security context");
		goto cleanup;
	}

	if (listen(listen_fd, 0) != 0) {
		wp_error("Failed to listen on the Unix socket for the security context");
		goto cleanup;
	}

	if (pipe(close_fd)) {
		wp_error("Failed to create a pipe for the security context");
		goto cleanup;
	}

	security_context = wp_security_context_manager_v1_create_listener(
			security_context_manager, listen_fd, close_fd[0]);
	if (security_context == NULL) {
		wp_error("Failed to create a security context listener");
		goto cleanup;
	}

	wp_security_context_v1_set_sandbox_engine(security_context, engine);
	wp_security_context_v1_set_instance_id(security_context, instance_id);
	wp_security_context_v1_set_app_id(security_context, app_id);
	wp_security_context_v1_commit(security_context);
	wp_security_context_v1_destroy(security_context);

	if (wl_display_roundtrip(display) < 0) {
		wp_error("Failed to execute display roundtrip");
		goto cleanup;
	}

	wp_debug("Successfully enabled Wayland security context");
	res = 0;

cleanup:

	if (res) {
		close_security_context();
	}
	if (security_context_manager) {
		wp_security_context_manager_v1_destroy(
				security_context_manager);
	}
	if (registry) {
		wl_registry_destroy(registry);
	}
	if (display) {
		wl_display_disconnect(display);
	}
	return res;
}
