// Vectorcom client — headless protocol relay (no UI).
// All presentation lives in the Rust TUI; this binary only bridges a TCP
// socket and stdin/stdout using the shared XOR line protocol:
//   stdin  lines  -> encrypt -> server
//   server bytes  -> decrypt -> stdout (verbatim, newline-framed)
#include <iostream>
#include <string>
#include <thread>
#include <vector>
#include <cstdint>
#include <cstring>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <signal.h>
#include <atomic>

using namespace std;

// Simple XOR encryption/decryption (must match server)
namespace Encryption
{
    const string KEY = "cracked-developer"; // Shared key (must match server)

    string encrypt(const string &plaintext)
    {
        string result = plaintext;
        for (size_t i = 0; i < result.length(); ++i)
        {
            result[i] ^= KEY[i % KEY.length()];
        }
        return result;
    }

    string decrypt(const string &ciphertext)
    {
        return encrypt(ciphertext); // XOR is symmetric
    }
}

static atomic<bool> g_running(true);

// Wire framing: [u32 big-endian length][N bytes XOR-encrypted payload]. Must
// match the server and the Rust shared::{read,write}_frame helpers.
static constexpr size_t MAX_FRAME_BYTES = 64 * 1024;

static bool recvN(int sock, char* buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = recv(sock, buf + got, n - got, 0);
        if (r <= 0) return false;
        got += static_cast<size_t>(r);
    }
    return true;
}

static bool recvFrame(int sock, string &payload) {
    uint8_t header[4];
    if (!recvN(sock, reinterpret_cast<char*>(header), 4)) return false;
    uint32_t len = (uint32_t(header[0]) << 24) | (uint32_t(header[1]) << 16)
                 | (uint32_t(header[2]) << 8)  |  uint32_t(header[3]);
    if (len > MAX_FRAME_BYTES) return false;
    vector<char> buf(len);
    if (len > 0 && !recvN(sock, buf.data(), len)) return false;
    payload = Encryption::decrypt(string(buf.data(), buf.size()));
    return true;
}

static bool sendFrame(int sock, const string &payload) {
    if (payload.size() > MAX_FRAME_BYTES) return false;
    string encrypted = Encryption::encrypt(payload);
    uint32_t len = static_cast<uint32_t>(payload.size());
    uint8_t header[4] = {
        static_cast<uint8_t>((len >> 24) & 0xFF),
        static_cast<uint8_t>((len >> 16) & 0xFF),
        static_cast<uint8_t>((len >> 8) & 0xFF),
        static_cast<uint8_t>(len & 0xFF),
    };
    string frame;
    frame.reserve(4 + encrypted.size());
    frame.append(reinterpret_cast<const char*>(header), 4);
    frame.append(encrypted);
    size_t sent = 0;
    while (sent < frame.size()) {
        ssize_t n = send(sock, frame.data() + sent, frame.size() - sent, 0);
        if (n <= 0) return false;
        sent += static_cast<size_t>(n);
    }
    return true;
}

// Decrypt incoming server frames and emit each payload verbatim to stdout so
// the front-end (Rust TUI) sees the raw protocol stream.
void receiveMessages(int sock) {
    while (true) {
        string payload;
        if (!recvFrame(sock, payload)) {
            g_running = false;
            close(sock);
            return;
        }
        cout.write(payload.data(), static_cast<streamsize>(payload.size()));
        cout.flush();
    }
}

int main(int argc, char* argv[]) {
    signal(SIGPIPE, SIG_IGN);
    if (argc < 2 || argc > 4) {
        cerr << "Usage: ./client [server_ip] <port> [username]" << endl;
        cerr << "  ./client 8080                       (local, username from stdin)" << endl;
        cerr << "  ./client 192.168.1.105 8080 alice   (remote, explicit username)" << endl;
        return 1;
    }

    string serverIp = "127.0.0.1";
    int port;
    string username;
    bool usernameProvided = false;

    try {
        if (argc == 2) {
            port = stoi(argv[1]);
        } else {
            serverIp = argv[1];
            port = stoi(argv[2]);
            if (argc == 4) {
                username = argv[3];
                usernameProvided = true;
            }
        }

        if (port < 1 || port > 65535) {
            cerr << "Error: Port must be between 1 and 65535" << endl;
            return 1;
        }
    } catch (const exception &e) {
        cerr << "Error: Invalid port number" << endl;
        return 1;
    }

    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) {
        cerr << "Failed to create socket." << endl;
        return 1;
    }

    struct sockaddr_in serverAddr{};
    serverAddr.sin_family = AF_INET;
    serverAddr.sin_port = htons(port);
    inet_pton(AF_INET, serverIp.c_str(), &serverAddr.sin_addr);

    if (connect(sock, (struct sockaddr*)&serverAddr, sizeof(serverAddr)) < 0) {
        cerr << "Connection failed to " << serverIp << ":" << port << endl;
        close(sock);
        return 1;
    }

    // Username: from argv if given, otherwise the first line on stdin.
    if (!usernameProvided) {
        if (!getline(cin, username)) {
            close(sock);
            return 0;
        }
    }
    if (username.empty()) username = "Anonymous";
    if (username.length() > 63) username = username.substr(0, 63);

    if (!sendFrame(sock, username)) {
        cerr << "Username send failed." << endl;
        close(sock);
        return 1;
    }

    thread receiver(receiveMessages, sock);
    receiver.detach();

    string message;
    while (getline(cin, message)) {
        if (!g_running.load()) break;

        if (message == "/quit") {
            close(sock);
            return 0;
        }
        if (message.empty()) continue;

        if (!sendFrame(sock, message)) {
            cerr << "Send failed." << endl;
            close(sock);
            return 1;
        }
    }

    close(sock);
    return 0;
}
