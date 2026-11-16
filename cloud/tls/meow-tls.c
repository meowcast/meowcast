/*
 * meow-tls HOST [PORT]: a verified TLS 1.2 connection, relayed to stdin/stdout.
 *
 * The dongle's cloud link (meow-netd, apps/netd/src/cloud.rs) runs this as a child
 * process and speaks HTTP/WebSocket through the pipes. Certificates are checked against the
 * root CAs compiled in from roots.pem (ta.h, made by BearSSL's "brssl ta"), the host name is
 * checked and sent as SNI. The clock must be set (NTP) first: certificates have validity dates.
 *
 * BearSSL (MIT, bearssl.org) does the TLS; this file is the socket/pipe loop around its engine,
 * modelled on BearSSL's tools/sslio.c. Exit status: 0 clean close, 1 usage/connect error,
 * 2 TLS error (the BearSSL error code is printed on stderr).
 */
#include <errno.h>
#include <netdb.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "bearssl.h"
#include "ta.h"

static int connect_to(const char *host, const char *port)
{
	struct addrinfo hints, *res, *p;
	int fd = -1, err;

	memset(&hints, 0, sizeof hints);
	hints.ai_family = AF_UNSPEC;
	hints.ai_socktype = SOCK_STREAM;
	err = getaddrinfo(host, port, &hints, &res);
	if (err != 0) {
		fprintf(stderr, "meow-tls: %s: %s\n", host, gai_strerror(err));
		return -1;
	}
	for (p = res; p != NULL; p = p->ai_next) {
		fd = socket(p->ai_family, p->ai_socktype, p->ai_protocol);
		if (fd < 0)
			continue;
		if (connect(fd, p->ai_addr, p->ai_addrlen) == 0)
			break;
		close(fd);
		fd = -1;
	}
	freeaddrinfo(res);
	if (fd < 0)
		fprintf(stderr, "meow-tls: cannot connect to %s:%s\n", host, port);
	return fd;
}

static int write_all(int fd, const unsigned char *buf, size_t len)
{
	while (len > 0) {
		ssize_t w = write(fd, buf, len);
		if (w < 0 && errno == EINTR)
			continue;
		if (w <= 0)
			return -1;
		buf += w;
		len -= (size_t)w;
	}
	return 0;
}

int main(int argc, char **argv)
{
	static br_ssl_client_context cc;
	static br_x509_minimal_context xc;
	static unsigned char iobuf[BR_SSL_BUFSIZE_BIDI];
	br_ssl_engine_context *eng = &cc.eng;
	int fd, in_open = 1, got_app = 0;

	if (argc < 2) {
		fprintf(stderr, "usage: meow-tls HOST [PORT]\n");
		return 1;
	}
	signal(SIGPIPE, SIG_IGN);
	fd = connect_to(argv[1], argc > 2 ? argv[2] : "443");
	if (fd < 0)
		return 1;

	br_ssl_client_init_full(&cc, &xc, TAs, TAs_NUM);
	br_ssl_engine_set_buffer(eng, iobuf, sizeof iobuf, 1);
	if (!br_ssl_client_reset(&cc, argv[1], 0)) {
		fprintf(stderr, "meow-tls: TLS reset failed (%d)\n", br_ssl_engine_last_error(eng));
		return 2;
	}

	for (;;) {
		unsigned st = br_ssl_engine_current_state(eng);
		struct pollfd pf[2];
		int n = 0, isock = -1, iin = -1;
		unsigned char *buf;
		size_t len;

		if (st & BR_SSL_CLOSED) {
			int err = br_ssl_engine_last_error(eng);
			if (err != BR_ERR_OK) {
				fprintf(stderr, "meow-tls: TLS error %d\n", err);
				return 2;
			}
			return 0;
		}
		/* Outgoing records first, then decrypted data for the reader. */
		if (st & BR_SSL_SENDREC) {
			buf = br_ssl_engine_sendrec_buf(eng, &len);
			ssize_t w = write(fd, buf, len);
			if (w < 0 && errno == EINTR)
				continue;
			if (w <= 0) {
				fprintf(stderr, "meow-tls: connection lost\n");
				return 1;
			}
			br_ssl_engine_sendrec_ack(eng, (size_t)w);
			continue;
		}
		if (st & BR_SSL_RECVAPP) {
			buf = br_ssl_engine_recvapp_buf(eng, &len);
			got_app = 1;
			if (write_all(1, buf, len) < 0)
				return 0; /* the reader went away */
			br_ssl_engine_recvapp_ack(eng, len);
			continue;
		}
		if (st & BR_SSL_RECVREC) {
			pf[n].fd = fd;
			pf[n].events = POLLIN;
			isock = n++;
		}
		if ((st & BR_SSL_SENDAPP) && in_open) {
			pf[n].fd = 0;
			pf[n].events = POLLIN;
			iin = n++;
		}
		if (n == 0) {
			if (!in_open && (st & BR_SSL_SENDAPP))
				continue; /* nothing to wait for: cannot happen while RECVREC is set */
			return 0;
		}
		if (poll(pf, (nfds_t)n, -1) < 0) {
			if (errno == EINTR)
				continue;
			return 1;
		}
		if (isock >= 0 && (pf[isock].revents & (POLLIN | POLLHUP | POLLERR))) {
			buf = br_ssl_engine_recvrec_buf(eng, &len);
			ssize_t r = read(fd, buf, len);
			if (r < 0 && errno == EINTR)
				continue;
			if (r <= 0) {
				/* Peer closed without close_notify: an error unless data already flowed. */
				int err = br_ssl_engine_last_error(eng);
				if (err != BR_ERR_OK || !got_app) {
					fprintf(stderr, "meow-tls: connection closed during the handshake (%d)\n", err);
					return 2;
				}
				return 0;
			}
			br_ssl_engine_recvrec_ack(eng, (size_t)r);
			continue;
		}
		if (iin >= 0 && (pf[iin].revents & (POLLIN | POLLHUP | POLLERR))) {
			buf = br_ssl_engine_sendapp_buf(eng, &len);
			ssize_t r = read(0, buf, len);
			if (r < 0 && errno == EINTR)
				continue;
			if (r <= 0) {
				/* Half close: stop sending, keep receiving until the peer closes. */
				in_open = 0;
				continue;
			}
			br_ssl_engine_sendapp_ack(eng, (size_t)r);
			br_ssl_engine_flush(eng, 0);
		}
	}
}
