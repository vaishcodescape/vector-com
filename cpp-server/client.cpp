// Vectorcom client — headless protocol relay (no UI).
// All presentation lives in the Rust TUI; this binary only bridges a TCP
// socket and stdin/stdout using the shared XOR line protocol:
//   stdin  lines  -> encrypt -> server
//   server bytes  -> decrypt -> stdout (verbatim, newline-framed)
#include <iostream>
#include <string>
#include <thread>
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
    const string KEY = "VectorcomSecureKey2025"; // Shared key (must match server)

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

// Decrypt incoming server bytes and emit them verbatim to stdout so the
// front-end (Rust TUI) sees the raw protocol stream.
void receiveMessages(int sock) {
    char buffer[1024];
    while (true) {
        memset(buffer, 0, sizeof(buffer));
        ssize_t bytes = recv(sock, buffer, sizeof(buffer) - 1, 0);
        if (bytes <= 0) {
            g_running = false;
            close(sock);
            return;
        }
        string line = Encryption::decrypt(string(buffer, bytes));
        cout.write(line.data(), static_cast<streamsize>(line.size()));
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

    send(sock, username.c_str(), username.size(), 0);

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

        string encrypted = Encryption::encrypt(message);
        ssize_t sent = send(sock, encrypted.c_str(), encrypted.size(), 0);
        if (sent < 0) {
            cerr << "Send failed." << endl;
            close(sock);
            return 1;
        }
    }

    close(sock);
    return 0;
}
