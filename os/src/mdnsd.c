/* mdnsd.c -- minimal IPv6-only mDNS / DNS-SD responder for one interface.
 *
 * The device is reachable only over IPv6 link-local, so this is what makes it
 * usable: the host resolves "<name>.local" to the fe80:: address *with the
 * right scope*, and browsing _pimpmypluto._tcp finds every attached radio.
 *
 *   mdnsd IFACE HOSTNAME [ALIAS] -- SERVICE:PORT ... [-- key=value ...]
 *   mdnsd usb0 pluto-463a pluto -- _pimpmypluto._tcp:7373 _ssh._tcp:22 -- serial=...
 *
 * Deliberate omissions: no probing/conflict resolution (the link is a
 * point-to-point USB cable and HOSTNAME carries the serial), no known-answer
 * suppression, no IPv4.  Any question naming one of our records is answered
 * with the full record set, which is legal and keeps this tiny.
 */
#include <arpa/inet.h>
#include <ctype.h>
#include <errno.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define MDNS_PORT 5353
#define TTL_HOST  120
#define TTL_SVC   4500
#define CLASS_IN  1
#define CACHE_FLUSH 0x8000
#define T_PTR 12
#define T_TXT 16
#define T_AAAA 28
#define T_SRV 33

struct svc { char type[64]; unsigned port; };

static const char *iface;
static unsigned ifindex;
static char host[64], alias[64];
static struct svc svcs[8];
static int nsvcs;
static uint8_t txt[256];
static size_t txtlen;

static int put_name(uint8_t *p, const char *name)
{
	uint8_t *start = p;
	while (*name) {
		const char *dot = strchr(name, '.');
		size_t n = dot ? (size_t)(dot - name) : strlen(name);
		if (n == 0 || n > 63)
			return -1;
		*p++ = (uint8_t)n;
		memcpy(p, name, n);
		p += n;
		name += n;
		if (*name == '.')
			name++;
	}
	*p++ = 0;
	return (int)(p - start);
}

/* Decode a possibly-compressed name at off; returns offset just past it in
 * the original stream, or -1. */
static int get_name(const uint8_t *pkt, size_t len, size_t off, char *out, size_t outlen)
{
	size_t o = 0, end = 0;
	int jumps = 0;
	for (;;) {
		uint8_t c;
		if (off >= len)
			return -1;
		c = pkt[off];
		if ((c & 0xc0) == 0xc0) {
			if (off + 1 >= len || ++jumps > 16)
				return -1;
			if (!end)
				end = off + 2;
			off = ((size_t)(c & 0x3f) << 8) | pkt[off + 1];
			continue;
		}
		if (c & 0xc0)
			return -1;
		off++;
		if (c == 0)
			break;
		if (off + c > len || o + c + 2 > outlen)
			return -1;
		if (o)
			out[o++] = '.';
		memcpy(out + o, pkt + off, c);
		o += c;
		off += c;
	}
	out[o] = 0;
	return (int)(end ? end : off);
}

static uint8_t *put_rr(uint8_t *p, const char *name, unsigned type, unsigned cls,
		       uint32_t ttl, uint8_t **rdlen)
{
	p += put_name(p, name);
	*p++ = type >> 8; *p++ = type;
	*p++ = cls >> 8;  *p++ = cls;
	*p++ = ttl >> 24; *p++ = ttl >> 16; *p++ = ttl >> 8; *p++ = ttl;
	*rdlen = p;
	return p + 2;
}

static void end_rr(uint8_t *rdlen, uint8_t *p)
{
	size_t n = (size_t)(p - rdlen - 2);
	rdlen[0] = n >> 8;
	rdlen[1] = n;
}

static int link_local(struct in6_addr *out)
{
	struct ifaddrs *ifa, *i;
	int found = 0;
	if (getifaddrs(&ifa) < 0)
		return 0;
	for (i = ifa; i; i = i->ifa_next) {
		struct sockaddr_in6 *s = (struct sockaddr_in6 *)i->ifa_addr;
		if (!s || s->sin6_family != AF_INET6 || strcmp(i->ifa_name, iface))
			continue;
		if (!IN6_IS_ADDR_LINKLOCAL(&s->sin6_addr))
			continue;
		*out = s->sin6_addr;
		found = 1;
		break;
	}
	freeifaddrs(ifa);
	return found;
}

static size_t build_response(uint8_t *pkt, uint16_t id, uint32_t ttl_scale)
{
	uint8_t *p = pkt + 12, *rl;
	struct in6_addr ll;
	char hostfq[80], name[160], inst[160];
	unsigned an = 0;
	int i;

	if (!link_local(&ll))
		return 0;
	memset(pkt, 0, 12);
	pkt[0] = id >> 8; pkt[1] = id;
	pkt[2] = 0x84;                    /* response, authoritative */

	snprintf(hostfq, sizeof hostfq, "%s.local", host);
	for (i = 0; i < nsvcs; i++) {
		snprintf(name, sizeof name, "%s.local", svcs[i].type);
		snprintf(inst, sizeof inst, "%s.%s.local", host, svcs[i].type);

		p = put_rr(p, "_services._dns-sd._udp.local", T_PTR, CLASS_IN, TTL_SVC * ttl_scale, &rl);
		p += put_name(p, name);
		end_rr(rl, p); an++;

		p = put_rr(p, name, T_PTR, CLASS_IN, TTL_SVC * ttl_scale, &rl);
		p += put_name(p, inst);
		end_rr(rl, p); an++;

		p = put_rr(p, inst, T_SRV, CLASS_IN | CACHE_FLUSH, TTL_HOST * ttl_scale, &rl);
		*p++ = 0; *p++ = 0; *p++ = 0; *p++ = 0;
		*p++ = svcs[i].port >> 8; *p++ = svcs[i].port;
		p += put_name(p, hostfq);
		end_rr(rl, p); an++;

		p = put_rr(p, inst, T_TXT, CLASS_IN | CACHE_FLUSH, TTL_SVC * ttl_scale, &rl);
		if (txtlen) {
			memcpy(p, txt, txtlen);
			p += txtlen;
		} else {
			*p++ = 0;
		}
		end_rr(rl, p); an++;
	}
	p = put_rr(p, hostfq, T_AAAA, CLASS_IN | CACHE_FLUSH, TTL_HOST * ttl_scale, &rl);
	memcpy(p, &ll, 16); p += 16;
	end_rr(rl, p); an++;
	if (alias[0]) {
		snprintf(name, sizeof name, "%s.local", alias);
		/* shared alias: no cache-flush, so two radios do not fight */
		p = put_rr(p, name, T_AAAA, CLASS_IN, TTL_HOST * ttl_scale, &rl);
		memcpy(p, &ll, 16); p += 16;
		end_rr(rl, p); an++;
	}
	pkt[6] = an >> 8; pkt[7] = an;
	return (size_t)(p - pkt);
}

static int ours(const char *q)
{
	char n[160];
	int i;
	snprintf(n, sizeof n, "%s.local", host);
	if (!strcasecmp(q, n))
		return 1;
	if (alias[0]) {
		snprintf(n, sizeof n, "%s.local", alias);
		if (!strcasecmp(q, n))
			return 1;
	}
	if (!strcasecmp(q, "_services._dns-sd._udp.local"))
		return 1;
	for (i = 0; i < nsvcs; i++) {
		snprintf(n, sizeof n, "%s.local", svcs[i].type);
		if (!strcasecmp(q, n))
			return 1;
		snprintf(n, sizeof n, "%s.%s.local", host, svcs[i].type);
		if (!strcasecmp(q, n))
			return 1;
	}
	return 0;
}

static void send_to(int fd, const uint8_t *pkt, size_t len, const struct sockaddr_in6 *to)
{
	struct sockaddr_in6 mc;
	if (!len)
		return;
	if (!to) {
		memset(&mc, 0, sizeof mc);
		mc.sin6_family = AF_INET6;
		mc.sin6_port = htons(MDNS_PORT);
		inet_pton(AF_INET6, "ff02::fb", &mc.sin6_addr);
		mc.sin6_scope_id = ifindex;
		to = &mc;
	}
	sendto(fd, pkt, len, 0, (const struct sockaddr *)to, sizeof *to);
}

int main(int argc, char **argv)
{
	struct sockaddr_in6 sa;
	struct ipv6_mreq mreq;
	uint8_t in[1500], out[1500];
	int fd, one = 1, hops = 255, zero = 0, i, announces = 3;

	if (argc < 3) {
		fprintf(stderr, "usage: mdnsd IFACE HOST [ALIAS] -- TYPE:PORT... [-- k=v...]\n");
		return 2;
	}
	iface = argv[1];
	snprintf(host, sizeof host, "%s", argv[2]);
	i = 3;
	if (i < argc && strcmp(argv[i], "--"))
		snprintf(alias, sizeof alias, "%s", argv[i++]);
	if (i < argc && !strcmp(argv[i], "--"))
		i++;
	for (; i < argc && strcmp(argv[i], "--") && nsvcs < 8; i++) {
		char *c = strrchr(argv[i], ':');
		if (!c)
			continue;
		snprintf(svcs[nsvcs].type, sizeof svcs[nsvcs].type, "%.*s",
			 (int)(c - argv[i]), argv[i]);
		svcs[nsvcs++].port = (unsigned)atoi(c + 1);
	}
	for (i++; i < argc; i++) {
		size_t n = strlen(argv[i]);
		if (n > 255 || txtlen + n + 1 > sizeof txt)
			continue;
		txt[txtlen++] = (uint8_t)n;
		memcpy(txt + txtlen, argv[i], n);
		txtlen += n;
	}

	/* The interface appears when the host configures the gadget. */
	while (!(ifindex = if_nametoindex(iface)))
		sleep(1);

	fd = socket(AF_INET6, SOCK_DGRAM, 0);
	setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
	setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
	memset(&sa, 0, sizeof sa);
	sa.sin6_family = AF_INET6;
	sa.sin6_port = htons(MDNS_PORT);
	if (bind(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
		perror("mdnsd: bind");
		return 1;
	}
	setsockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_IF, &ifindex, sizeof ifindex);
	setsockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops, sizeof hops);
	setsockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &zero, sizeof zero);
	memset(&mreq, 0, sizeof mreq);
	inet_pton(AF_INET6, "ff02::fb", &mreq.ipv6mr_multiaddr);
	mreq.ipv6mr_interface = ifindex;
	/* Joining fails until the link is up; keep trying. */
	while (setsockopt(fd, IPPROTO_IPV6, IPV6_JOIN_GROUP, &mreq, sizeof mreq) < 0)
		sleep(1);

	for (;;) {
		struct pollfd pfd = { fd, POLLIN, 0 };
		int r = poll(&pfd, 1, announces ? 1000 : -1);

		if (r == 0 && announces) {
			size_t n = build_response(out, 0, 1);
			if (n) {
				send_to(fd, out, n, NULL);
				announces--;
			}
			continue;
		}
		if (r > 0) {
			struct sockaddr_in6 from;
			socklen_t fl = sizeof from;
			ssize_t len = recvfrom(fd, in, sizeof in, 0, (struct sockaddr *)&from, &fl);
			unsigned qd, q;
			size_t off = 12;
			int hit = 0;

			if (len < 12 || (in[2] & 0x80))      /* too short, or a response */
				continue;
			qd = (unsigned)in[4] << 8 | in[5];
			for (q = 0; q < qd && !hit; q++) {
				char name[256];
				int e = get_name(in, (size_t)len, off, name, sizeof name);
				if (e < 0 || (size_t)e + 4 > (size_t)len)
					break;
				off = (size_t)e + 4;
				hit = ours(name);
			}
			if (!hit)
				continue;
			if (ntohs(from.sin6_port) != MDNS_PORT) {
				/* legacy unicast resolver: reply directly, echo its id */
				size_t n = build_response(out, (uint16_t)(in[0] << 8 | in[1]), 1);
				send_to(fd, out, n, &from);
			} else {
				send_to(fd, out, build_response(out, 0, 1), NULL);
			}
		}
	}
}
