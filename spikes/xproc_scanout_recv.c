// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// System-compositor spike, receiver half: take DRM master on a card, accept ONE dmabuf from
// another process over a unix socket (see synoik-vk/examples/xproc_scanout_send.rs), PRIME-import
// it, AddFB2 it and keep flipping it for <secs>, outliving the sender.
//
// Build: gcc -O2 -o xproc_scanout_recv xproc_scanout_recv.c $(pkg-config --cflags --libs libdrm)
// Usage: xproc_scanout_recv <card> <socket> <secs>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>

static double now(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void on_flip(int fd, unsigned seq, unsigned s, unsigned us, void *data) {
	(void)fd; (void)seq; (void)s; (void)us;
	*(int *)data = 0;
}

int main(int argc, char **argv) {
	if (argc != 4) { fprintf(stderr, "usage: %s <card> <socket> <secs>\n", argv[0]); return 2; }
	int secs = atoi(argv[3]);
	int card = open(argv[1], O_RDWR | O_CLOEXEC);
	if (card < 0) { perror("open card"); return 1; }
	if (drmSetMaster(card)) { perror("drmSetMaster"); return 1; }
	drmSetClientCap(card, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);

	drmModeRes *res = drmModeGetResources(card);
	drmModeConnector *conn = NULL;
	for (int i = 0; i < res->count_connectors; i++) {
		conn = drmModeGetConnector(card, res->connectors[i]);
		if (conn->connection == DRM_MODE_CONNECTED && conn->count_modes) break;
		drmModeFreeConnector(conn); conn = NULL;
	}
	if (!conn) { fprintf(stderr, "no connected connector\n"); return 1; }
	uint32_t crtc = res->crtcs[0];
	drmModeModeInfo mode = conn->modes[0];
	printf("recv: connector %u mode %s, crtc %u\n", conn->connector_id, mode.name, crtc);

	int ls = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct sockaddr_un sa = { .sun_family = AF_UNIX };
	strncpy(sa.sun_path, argv[2], sizeof sa.sun_path - 1);
	unlink(argv[2]);
	if (bind(ls, (struct sockaddr *)&sa, sizeof sa) || listen(ls, 1)) { perror("bind/listen"); return 1; }
	printf("recv: waiting on %s\n", argv[2]); fflush(stdout);
	int cs = accept(ls, NULL, NULL);

	uint8_t hdr[24];
	char cbuf[CMSG_SPACE(sizeof(int))];
	struct iovec iov = { hdr, sizeof hdr };
	struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
	if (recvmsg(cs, &msg, 0) != sizeof hdr) { perror("recvmsg"); return 1; }
	struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
	int dmabuf; memcpy(&dmabuf, CMSG_DATA(c), sizeof dmabuf);
	uint32_t w, h, stride, offset; uint64_t modifier;
	memcpy(&w, hdr, 4); memcpy(&h, hdr + 4, 4); memcpy(&stride, hdr + 8, 4); memcpy(&offset, hdr + 12, 4);
	memcpy(&modifier, hdr + 16, 8);
	printf("recv: dmabuf fd %d %ux%u stride %u offset %u modifier %#llx\n", dmabuf, w, h, stride, offset,
	       (unsigned long long)modifier);

	uint32_t handle;
	if (drmPrimeFDToHandle(card, dmabuf, &handle)) { perror("drmPrimeFDToHandle"); return 1; }
	uint32_t handles[4] = { handle }, pitches[4] = { stride }, offsets[4] = { offset };
	uint64_t mods[4] = { modifier };
	uint32_t fb;
	if (drmModeAddFB2WithModifiers(card, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets, mods, &fb,
	                               DRM_MODE_FB_MODIFIERS)) {
		perror("AddFB2WithModifiers (retrying without modifiers)");
		if (drmModeAddFB2(card, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets, &fb, 0)) {
			perror("AddFB2"); return 1;
		}
	}
	close(dmabuf);
	if (drmModeSetCrtc(card, crtc, fb, 0, 0, &conn->connector_id, 1, &mode)) { perror("SetCrtc"); return 1; }
	printf("recv: fb %u on screen\n", fb); fflush(stdout);

	drmEventContext ev = { .version = 2, .page_flip_handler = on_flip };
	double end = now() + secs, last_report = 0;
	int flips = 0, errors = 0;
	while (now() < end) {
		int pending = 1;
		if (drmModePageFlip(card, crtc, fb, DRM_MODE_PAGE_FLIP_EVENT, &pending)) {
			if (errors++ < 5) fprintf(stderr, "recv: page flip: %s\n", strerror(errno));
			usleep(16000);
			continue;
		}
		while (pending) {
			struct pollfd p = { card, POLLIN, 0 };
			if (poll(&p, 1, 1000) <= 0) { fprintf(stderr, "recv: flip event timeout\n"); break; }
			drmHandleEvent(card, &ev);
		}
		flips++;
		if (now() - last_report >= 1.0) {
			printf("recv: %d flips, %d errors\n", flips, errors); fflush(stdout);
			last_report = now();
		}
	}
	printf("recv: done, %d flips, %d errors\n", flips, errors);
	return 0;
}
