// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

#include <stdio.h>
#include <fcntl.h>
#include <errno.h>
#include <stdlib.h>
#include <unistd.h>
#include <string.h>
#include <strings.h>
#include <sys/sockio.h>
#include <net/if.h>
#include <arpa/inet.h>
#include <sys/stropts.h>

#include <sys/types.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/ip6.h>

int
bind_to_iface(int32_t socket, uint32_t ifindex)
{
	return (setsockopt(socket, IPPROTO_IPV6, IPV6_BOUND_IF, &ifindex,
	    sizeof(ifindex)));
}
