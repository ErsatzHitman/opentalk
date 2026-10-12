/* The pinned sherpa servers have no bind-address flag. Only their WS2_32 imports use this
 * shim: other functions forward unchanged, while every listener is confined to loopback. */
#include <winsock2.h>
#include <ws2tcpip.h>

int WSAAPI ot_bind(SOCKET socket, const struct sockaddr *address, int length) {
    if (!address || length < (int)sizeof(address->sa_family)) {
        WSASetLastError(WSAEFAULT);
        return SOCKET_ERROR;
    }
    if (address->sa_family == AF_INET && length >= (int)sizeof(struct sockaddr_in)) {
        struct sockaddr_in local = *(const struct sockaddr_in *)address;
        local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        return bind(socket, (const struct sockaddr *)&local, sizeof(local));
    }
    if (address->sa_family == AF_INET6 && length >= (int)sizeof(struct sockaddr_in6)) {
        struct sockaddr_in6 local = *(const struct sockaddr_in6 *)address;
        local.sin6_addr = in6addr_loopback;
        local.sin6_scope_id = 0;
        return bind(socket, (const struct sockaddr *)&local, sizeof(local));
    }
    WSASetLastError(WSAEAFNOSUPPORT);
    return SOCKET_ERROR;
}
