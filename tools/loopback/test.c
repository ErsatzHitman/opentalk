#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <stdio.h>

typedef int (WSAAPI *bind_fn)(SOCKET, const struct sockaddr *, int);
#define CHECK(ok) do { if (!(ok)) { fprintf(stderr, "failed line %d, WSA %d\n", __LINE__, WSAGetLastError()); return 1; } } while (0)

int main(void) {
    WSADATA data;
    CHECK(WSAStartup(MAKEWORD(2,2), &data) == 0);
    HMODULE shim = LoadLibraryA("ot-net.dll");
    CHECK(shim != NULL);
    bind_fn restricted = (bind_fn)GetProcAddress(shim, "bind");
    CHECK(restricted != NULL);
    CHECK((FARPROC)restricted == GetProcAddress(shim, MAKEINTRESOURCEA(2)));
    /* These are precisely the ordinal imports in both pinned server executables. */
    HMODULE original = GetModuleHandleA("WS2_32.dll");
    CHECK(original != NULL);
    int ordinals[] = {3,5,7,8,9,10,13,14,15,21,22,111,112,115,116};
    for (unsigned i = 0; i < sizeof(ordinals)/sizeof(ordinals[0]); ++i) {
        CHECK(GetProcAddress(shim, MAKEINTRESOURCEA(ordinals[i])) == GetProcAddress(original, MAKEINTRESOURCEA(ordinals[i])));
    }
    const char *names[] = {"WSASend", "WSARecv", "WSASocketW", "WSAAddressToStringW"};
    for (unsigned i = 0; i < sizeof(names)/sizeof(names[0]); ++i) CHECK(GetProcAddress(shim, names[i]) == GetProcAddress(original, names[i]));
    unsigned long addresses[] = {INADDR_ANY, 0xc0000201};
    for (unsigned i = 0; i < 2; ++i) {
        SOCKET s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        CHECK(s != INVALID_SOCKET);
        struct sockaddr_in addr = {0};
        addr.sin_family = AF_INET;
        addr.sin_addr.s_addr = htonl(addresses[i]);
        CHECK(restricted(s, (struct sockaddr *)&addr, sizeof(addr)) == 0);
        CHECK(listen(s, 1) == 0);
        int len = sizeof(addr);
        CHECK(getsockname(s, (struct sockaddr *)&addr, &len) == 0);
        CHECK(ntohl(addr.sin_addr.s_addr) == INADDR_LOOPBACK);
        CHECK(closesocket(s) == 0);
    }
    SOCKET s = socket(AF_INET6, SOCK_STREAM, IPPROTO_TCP);
    CHECK(s != INVALID_SOCKET);
    struct sockaddr_in6 addr6 = {0};
    addr6.sin6_family = AF_INET6;
    CHECK(restricted(s, (struct sockaddr *)&addr6, sizeof(addr6)) == 0);
    int len = sizeof(addr6);
    CHECK(getsockname(s, (struct sockaddr *)&addr6, &len) == 0);
    CHECK(IN6_IS_ADDR_LOOPBACK(&addr6.sin6_addr));
    CHECK(closesocket(s) == 0);
    CHECK(restricted(INVALID_SOCKET, NULL, 0) == SOCKET_ERROR);
    CHECK(WSAGetLastError() == WSAEFAULT);
    FreeLibrary(shim);
    WSACleanup();
    puts("IPv4 and IPv6 listeners confined to loopback; pinned-server exports available");
    return 0;
}
