// Vector-Com Server with enhanced features and security
/*Features: - Secure communication with XOR encryption
            - Room-based chat
            - User blocking
            - Slowmode per room
            - Admin console */
            
#include <iostream>
#include <vector>
#include <thread>
#include <mutex>
#include <string>
#include <cstring>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <signal.h>
#include <algorithm>
#include <chrono>
#include <iomanip>
#include <sstream>
#include <fstream>
#include <unordered_map>
#include <unordered_set>
#include <sys/types.h>
#include <sys/stat.h>

using namespace std;

// Simple XOR encryption/decryption
namespace Encryption
{
    const string KEY = "cracked-developer"; // Shared key
    
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

struct ClientInfo
{
    int socket;
    string name;
    string addr;
    string room = "general";

    // Rate limit: rolling 1-second window starting at rateWindowStart.
    chrono::steady_clock::time_point rateWindowStart = chrono::steady_clock::now();
    int msgCount = 0;
    // Slowmode: timestamp of the last accepted broadcast message in the current
    // room. Tracked separately so the rate-limit window can't reset it.
    chrono::steady_clock::time_point lastBroadcastTime{};
    ClientInfo(int s, const string &n, const string &a, const string &r)
        : socket(s), name(n), addr(a), room(r) {}
};

class ChatServer
{
private:
    int serverSocket;
    int port;
    vector<ClientInfo> clients;
    mutex clientsMutex;
    bool running;
    unordered_map<string, int> roomSlowmodeSeconds; // seconds per room
    unordered_set<string> blockedUsers; // admin-managed: blocked users cannot send messages
    // Shared secret for `/admin` over the socket. Read from the
    // VECTORCOM_ADMIN_TOKEN env var at startup; empty disables admin-over-wire.
    string adminToken;

public:
    ChatServer(int port) : port(port), running(false)
    {
        if (const char* tok = getenv("VECTORCOM_ADMIN_TOKEN"))
            adminToken = tok;

        serverSocket = socket(AF_INET, SOCK_STREAM, 0);
        if (serverSocket < 0)
            throw runtime_error("Failed to create socket");

        int opt = 1;
        if (setsockopt(serverSocket, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt)) < 0)
            throw runtime_error("Failed to set socket options");
    }

    ~ChatServer()
    {
        stop();
        if (serverSocket >= 0)
            close(serverSocket);
    }

    void start()
    {
        sockaddr_in serverAddr{};
        serverAddr.sin_family = AF_INET;
        serverAddr.sin_addr.s_addr = INADDR_ANY;
        serverAddr.sin_port = htons(port);

        if (::bind(serverSocket, (sockaddr *)&serverAddr, sizeof(serverAddr)) < 0)
            throw runtime_error("Failed to bind socket to port " + to_string(port));

        if (listen(serverSocket, 10) < 0)
            throw runtime_error("Failed to listen on socket");

        running = true;

        cout << "vectorcom server listening on port " << port << endl;
        cout << "admin commands available: type 'help' for options" << endl;
        if (adminToken.empty())
            cout << "admin-over-socket DISABLED (set VECTORCOM_ADMIN_TOKEN to enable)" << endl;
        else
            cout << "admin-over-socket enabled (token via VECTORCOM_ADMIN_TOKEN)" << endl;

        thread(&ChatServer::adminConsole, this).detach();

        while (running)
        {
            sockaddr_in clientAddr{};
            socklen_t clientAddrLen = sizeof(clientAddr);
            int clientSocket = accept(serverSocket, (sockaddr *)&clientAddr, &clientAddrLen);
            if (clientSocket < 0)
                continue;

            string clientIp = inet_ntoa(clientAddr.sin_addr);
            int clientPort = ntohs(clientAddr.sin_port);
            string addrStr = clientIp + ":" + to_string(clientPort);

            thread(&ChatServer::handleClient, this, clientSocket, addrStr).detach();
        }
    }

    void stop()
    {
        running = false;
        if (serverSocket >= 0)
        {
            // Make accept() return immediately; the listener fd is owned by
            // start()'s thread and will be closed there or in the destructor.
            shutdown(serverSocket, SHUT_RDWR);
        }
        // Signal each client's owning thread to wake from recv(). The owners
        // (handleClient) close the fds and remove their entries — closing here
        // would race with their recv() and risk closing a reused fd.
        lock_guard<mutex> lock(clientsMutex);
        for (auto &c : clients)
            shutdown(c.socket, SHUT_RDWR);
    }

private:
    // Hard cap on a single framed payload: must match shared::MAX_FRAME_BYTES on
    // the Rust side. The XOR transport isn't authenticated, so a corrupt or
    // hostile peer could otherwise convince us to allocate gigabytes.
    static constexpr size_t MAX_FRAME_BYTES = 64 * 1024;

    // Wire format: [u32 big-endian length][N bytes XOR-encrypted payload].
    // The length prefix is plaintext so the receiver can always find frame
    // boundaries; the XOR offset resets per frame, so TCP segment coalescing
    // no longer desyncs the keystream.
    static bool sendAll(int sock, const char* data, size_t len)
    {
        if (len > MAX_FRAME_BYTES) return false;

        string encrypted = Encryption::encrypt(string(data, len));

        uint8_t header[4];
        uint32_t lenBe = static_cast<uint32_t>(len);
        header[0] = (lenBe >> 24) & 0xFF;
        header[1] = (lenBe >> 16) & 0xFF;
        header[2] = (lenBe >> 8) & 0xFF;
        header[3] = lenBe & 0xFF;

        // One contiguous buffer keeps the prefix and body together — both for
        // efficiency and so a write failure between them can't leave a peer
        // mid-frame with no recovery path.
        string frame;
        frame.reserve(4 + encrypted.size());
        frame.append(reinterpret_cast<const char*>(header), 4);
        frame.append(encrypted);

        size_t totalSent = 0;
        while (totalSent < frame.size())
        {
            ssize_t n = send(sock, frame.data() + totalSent, frame.size() - totalSent, 0);
            if (n <= 0) return false;
            totalSent += static_cast<size_t>(n);
        }
        return true;
    }

    static bool recvN(int sock, char* buf, size_t n)
    {
        size_t got = 0;
        while (got < n)
        {
            ssize_t r = recv(sock, buf + got, n - got, 0);
            if (r <= 0) return false;
            got += static_cast<size_t>(r);
        }
        return true;
    }

    // Read one frame. Returns false on EOF or framing error; sets `payload` to
    // the decrypted body.
    static bool recvFrame(int sock, string &payload)
    {
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

    static void ensureHistoryDir()
    {
        struct stat st{};
        if (stat("history", &st) != 0)
        {
            mkdir("history", 0755);
        }
    }

    static string nowTimestamp()
    {
        using namespace chrono;
        auto t = system_clock::now();
        auto tt = system_clock::to_time_t(t);
        tm local_tm;
        localtime_r(&tt, &local_tm);
        stringstream ss;
        ss << put_time(&local_tm, "%H:%M:%S");
        return ss.str();
    }

    void adminConsole()
    {
        string cmd;
        while (running)
        {
            getline(cin, cmd);
            if (!running)
                break;

            if (cmd.rfind("kick ", 0) == 0)
            {
                string user = cmd.substr(5);
                kickUser(user);
            }
            else if (cmd.rfind("say ", 0) == 0)
            {
                string msg = "[SERVER] " + cmd.substr(4);
                broadcastMessage(msg, -1, "");
            }
            else if (cmd.rfind("block ", 0) == 0)
            {
                string user = cmd.substr(6);
                {
                    lock_guard<mutex> lock(clientsMutex);
                    blockedUsers.insert(user);
                }
                notifyUser(user, "[SERVER] You have been blocked by the admin.");
                cout << "Blocked user: " << user << endl;
            }
            else if (cmd.rfind("unblock ", 0) == 0)
            {
                string user = cmd.substr(8);
                {
                    lock_guard<mutex> lock(clientsMutex);
                    blockedUsers.erase(user);
                }
                notifyUser(user, "[SERVER] You have been unblocked by the admin.");
                cout << "Unblocked user: " << user << endl;
            }
            else if (cmd.rfind("slowmode ", 0) == 0)
            {
                // slowmode <room> <seconds>
                istringstream iss(cmd.substr(9));
                string room; int seconds = 0;
                if (iss >> room >> seconds)
                {
                    {
                        lock_guard<mutex> lock(clientsMutex);
                        roomSlowmodeSeconds[room] = max(0, seconds);
                    }
                    string notice = "[SERVER] Slowmode for room '" + room + "' set to " + to_string(seconds) + "s";
                    broadcastMessage(notice, -1, room);
                    cout << notice << endl;
                }
                else
                {
                    cout << "Usage: slowmode <room> <seconds>" << endl;
                }
            }
            else if (cmd == "list")
            {
                lock_guard<mutex> lock(clientsMutex);
                cout << "Connected users:" << endl;
                for (auto &c : clients)
                    cout << "  - " << c.name << " (" << c.addr << ") room=" << c.room << endl;
            }
            else if (cmd == "help")
            {
                cout << "Admin commands:\n";
                cout << "  kick <username>       - Kick a user\n";
                cout << "  say <message>         - Broadcast to all rooms\n";
                cout << "  block <username>      - Block a user from sending messages\n";
                cout << "  unblock <username>    - Unblock a user\n";
                cout << "  slowmode <room> <sec> - Set room slowmode\n";
                cout << "  list                  - List online users\n";
                cout << "  help                  - Show this help\n";
            }
        }
    }

    void saveMessage(const string &room, const string &message)
    {
        ensureHistoryDir();
        ofstream file("history/history_" + room + ".txt", ios::app);
        if (file.is_open())
            file << message << endl;
    }

    void sendRoomHistory(int clientSocket, const string &room)
    {
        ensureHistoryDir();
        ifstream file("history/history_" + room + ".txt");
        if (!file.is_open())
            return;
        string line;
        string header = "---- Chat History for room '" + room + "' ----\n";
        sendAll(clientSocket, header.c_str(), header.size());
        while (getline(file, line))
        {
            sendAll(clientSocket, line.c_str(), line.size());
            sendAll(clientSocket, "\n", 1);
        }
        string footer = "-------------------------------------------\n";
        sendAll(clientSocket, footer.c_str(), footer.size());
    }

    bool isRateLimited(int clientSocket)
    {
        lock_guard<mutex> lock(clientsMutex);

        for (auto &c : clients)
        {
            if (c.socket == clientSocket)
            {
                auto now = chrono::steady_clock::now();
                auto diff = chrono::duration_cast<chrono::milliseconds>(now - c.rateWindowStart).count();

                if (diff > 1000)
                {
                    c.msgCount = 0;
                    c.rateWindowStart = now;
                }

                c.msgCount++;

                return c.msgCount > 3;
            }
        }
        return false;
    }

    void handleClient(int clientSocket, string addrStr)
    {
        string username;
        if (!recvFrame(clientSocket, username))
        {
            close(clientSocket);
            return;
        }

        while (!username.empty() && (username.back() == '\n' || username.back() == '\r'))
            username.pop_back();
        if (username.empty())
            username = "Anonymous";

        // Truncate username if too long (safety check)
        if (username.length() > 63)
            username = username.substr(0, 63);

        {
            lock_guard<mutex> lock(clientsMutex);
            clients.push_back({clientSocket, username, addrStr, "general"});
        }

        string joinMsg = "[" + nowTimestamp() + "] " + username + " joined the chat (room: general)";
        cout << joinMsg << endl;
        broadcastMessage(joinMsg, clientSocket, "general");
        saveMessage("general", joinMsg);
        sendRoomHistory(clientSocket, "general");

        while (running)
        {
            string msg;
            if (!recvFrame(clientSocket, msg))
            {
                string leftRoom = getClientRoom(clientSocket);
                close(clientSocket);
                removeClient(clientSocket);
                string leftMsg = "[" + nowTimestamp() + "] " + username + " left the chat";
                cout << leftMsg << endl;
                broadcastMessage(leftMsg, -1, leftRoom);
                saveMessage(leftRoom, leftMsg);
                break;
            }

            while (!msg.empty() && (msg.back() == '\n' || msg.back() == '\r'))
                msg.pop_back();
            if (msg.empty())
                continue;

            if (msg == "/rooms")
            {
                lock_guard<mutex> lock(clientsMutex);

                unordered_map<string, int> roomCount;
                for (auto &c : clients)
                {
                    roomCount[c.room]++;
                }

                string out = "Active Rooms:\n";
                for (auto &r : roomCount)
                {
                    out += " - " + r.first + " (" + to_string(r.second) + " users)\n";
                }

                sendAll(clientSocket, out.c_str(), out.size());
                continue;
            }

            if (msg == "/help")
            {
                string help =
                    "Available Commands:\n"
                    "/list               - List online users\n"
                    "/rooms              - List all active rooms\n"
                    "/join <room>        - Join or create a room\n"
                    "/pm <user> <msg>    - Private message\n"
                    "/pin <msg>          - Pin a message to the room board\n"
                    "/pins               - Show pinned messages for the room\n"
                    "/unpin <index>      - Remove a pinned message by index\n"
                    "/quit               - Disconnect from server\n"
                    "/help               - Show this help\n";

                help += "\n"; // <- IMPORTANT: ensures it prints immediately
                sendAll(clientSocket, help.c_str(), help.size());
                continue;
            }

            if (msg == "/list")
            {
                string listMsg = "Online users:\n";
                lock_guard<mutex> lock(clientsMutex);
                for (auto &c : clients)
                    listMsg += " - " + c.name + " (room: " + c.room + ")\n";
                sendAll(clientSocket, listMsg.c_str(), listMsg.size());
                continue;
            }

            if (msg.rfind("/join ", 0) == 0)
            {
                string newRoom = msg.substr(6);
                while (!newRoom.empty() && isspace(static_cast<unsigned char>(newRoom.front())))
                    newRoom.erase(newRoom.begin());
                while (!newRoom.empty() && isspace(static_cast<unsigned char>(newRoom.back())))
                    newRoom.pop_back();
                if (newRoom.empty())
                {
                    string err = "Usage: /join <room>\n";
                    sendAll(clientSocket, err.c_str(), err.size());
                    continue;
                }

                string oldRoom;

                {
                    lock_guard<mutex> lock(clientsMutex);
                    for (auto &c : clients)
                    {
                        if (c.socket == clientSocket)
                        {
                            oldRoom = c.room;
                        }
                    }
                }

                if (oldRoom != newRoom)
                {

                    string leftMsg = "[" + nowTimestamp() + "] " + username + " left room " + oldRoom;
                    broadcastMessage(leftMsg, clientSocket, oldRoom);
                    saveMessage(oldRoom, leftMsg);

                    {
                        lock_guard<mutex> lock(clientsMutex);
                        for (auto &c : clients)
                        {
                            if (c.socket == clientSocket)
                            {
                                c.room = newRoom;
                            }
                        }
                    }

                    string joinMsg = "[" + nowTimestamp() + "] " + username + " joined room " + newRoom;
                    broadcastMessage(joinMsg, clientSocket, newRoom);
                    saveMessage(newRoom, joinMsg);

                    sendRoomHistory(clientSocket, newRoom);
                }

                continue;
            }

            if (msg.rfind("/block", 0) == 0 || msg.rfind("/unblock", 0) == 0 || msg == "/blocklist")
            {
                string err = "Blocking is managed by the server admin.\n";
                sendAll(clientSocket, err.c_str(), err.size());
                continue;
            }

            if (msg.rfind("/pin ", 0) == 0)
            {
                string room = getClientRoom(clientSocket);
                string text = msg.substr(5);
                if (text.empty())
                {
                    string err = "Usage: /pin <message>\n";
                    sendAll(clientSocket, err.c_str(), err.size());
                    continue;
                }
                ensureHistoryDir();
                string formatted = "📌 [" + nowTimestamp() + "] " + username + ": " + text;
                {
                    ofstream pf("history/pins_" + room + ".txt", ios::app);
                    if (pf.is_open()) pf << formatted << endl;
                }
                string notice = "[" + nowTimestamp() + "] " + username + " pinned a message.";
                broadcastMessage(notice, -1, room);
                sendAll(clientSocket, "Pinned.\n", 8);
                continue;
            }

            if (msg == "/pins")
            {
                string room = getClientRoom(clientSocket);
                ensureHistoryDir();
                ifstream pf("history/pins_" + room + ".txt");
                if (!pf.is_open())
                {
                    string none = "No pins yet.\n";
                    sendAll(clientSocket, none.c_str(), none.size());
                    continue;
                }
                string line;
                string header = "Pinned messages in '" + room + "':\n";
                sendAll(clientSocket, header.c_str(), header.size());
                int idx = 1;
                while (getline(pf, line))
                {
                    string entry = to_string(idx++) + ". " + line + "\n";
                    sendAll(clientSocket, entry.c_str(), entry.size());
                }
                continue;
            }

            if (msg.rfind("/unpin ", 0) == 0)
            {
                string room = getClientRoom(clientSocket);
                string idxStr = msg.substr(7);
                int idx = 0;
                try { idx = stoi(idxStr); } catch (...) { idx = 0; }
                if (idx <= 0)
                {
                    string err = "Usage: /unpin <index>\n";
                    sendAll(clientSocket, err.c_str(), err.size());
                    continue;
                }
                ensureHistoryDir();
                string path = "history/pins_" + room + ".txt";
                ifstream pf(path);
                if (!pf.is_open())
                {
                    string none = "No pins to unpin.\n";
                    sendAll(clientSocket, none.c_str(), none.size());
                    continue;
                }
                vector<string> all;
                string line;
                while (getline(pf, line)) all.push_back(line);
                pf.close();
                if (idx > (int)all.size())
                {
                    string err = "Invalid index.\n";
                    sendAll(clientSocket, err.c_str(), err.size());
                    continue;
                }
                all.erase(all.begin() + (idx - 1));
                ofstream wf(path, ios::trunc);
                for (auto &l : all) wf << l << "\n";
                string ok = "Unpinned #" + to_string(idx) + ".\n";
                sendAll(clientSocket, ok.c_str(), ok.size());
                continue;
            }

            if (msg.rfind("/pm ", 0) == 0)
            {
                string rest = msg.substr(4);
                size_t space = rest.find(' ');
                if (space == string::npos)
                {
                    string err = "Usage: /pm <username> <message>\n";
                    sendAll(clientSocket, err.c_str(), err.size());
                    continue;
                }
                string targetUser = rest.substr(0, space);
                string privateMsg = rest.substr(space + 1);
                sendPrivateMessage(username, targetUser, privateMsg);
                continue;
            }

            // Admin controls issued over the socket (used by the Rust dashboard).
            // Note: unauthenticated — intended for a trusted local operator, in
            // keeping with this project's educational scope.
            if (msg.rfind("/admin ", 0) == 0)
            {
                handleAdminCommand(msg.substr(7), clientSocket, username);
                continue;
            }

            if (isRateLimited(clientSocket))
            {
                string warn = "⚠️ Rate limit exceeded. Slow down!\n";
                sendAll(clientSocket, warn.c_str(), warn.size());
                continue;
            }

            // Room slowmode check
            {
                string room = getClientRoom(clientSocket);
                int slowSeconds = 0;
                {
                    lock_guard<mutex> lock(clientsMutex);
                    auto it = roomSlowmodeSeconds.find(room);
                    if (it != roomSlowmodeSeconds.end()) slowSeconds = it->second;
                }
                if (slowSeconds > 0)
                {
                    bool blocked = false;
                    long remaining = 0;
                    {
                        lock_guard<mutex> lock(clientsMutex);
                        for (auto &c : clients)
                        {
                            if (c.socket == clientSocket)
                            {
                                auto now = chrono::steady_clock::now();
                                // Zero-init means "never sent"; any positive
                                // slowmode then allows the first message.
                                if (c.lastBroadcastTime.time_since_epoch().count() != 0)
                                {
                                    auto diff = chrono::duration_cast<chrono::seconds>(now - c.lastBroadcastTime).count();
                                    if (diff < slowSeconds)
                                    {
                                        blocked = true;
                                        remaining = slowSeconds - diff;
                                    }
                                }
                                break;
                            }
                        }
                    }
                    if (blocked)
                    {
                        string warn = "⌛ Slowmode is on (" + to_string(slowSeconds) + "s). Wait " + to_string(remaining) + "s.\n";
                        sendAll(clientSocket, warn.c_str(), warn.size());
                        continue;
                    }
                }
            }

            {
                lock_guard<mutex> lock(clientsMutex);
                if (blockedUsers.count(username))
                {
                    string warn = "You are blocked by the admin and cannot send messages.\n";
                    sendAll(clientSocket, warn.c_str(), warn.size());
                    continue;
                }
            }

            string formatted = "[" + nowTimestamp() + "] " + username + ": " + msg;
            cout << formatted << endl;
            string clientRoom = getClientRoom(clientSocket);
            {
                lock_guard<mutex> lock(clientsMutex);
                for (auto &c : clients)
                {
                    if (c.socket == clientSocket)
                    {
                        c.lastBroadcastTime = chrono::steady_clock::now();
                        break;
                    }
                }
            }
            broadcastMessage(formatted, clientSocket, clientRoom);
            saveMessage(clientRoom, formatted);
        }
    }

    string getClientRoom(int sock)
    {
        lock_guard<mutex> lock(clientsMutex);
        for (auto &c : clients)
            if (c.socket == sock)
                return c.room;
        return "general";
    }

    void sendPrivateMessage(const string &fromUser, const string &toUser, const string &msg)
    {
        lock_guard<mutex> lock(clientsMutex);
        int fromSock = -1;
        for (auto &c : clients)
        {
            if (c.name == fromUser) fromSock = c.socket;
        }
        if (blockedUsers.count(fromUser))
        {
            if (fromSock != -1)
            {
                string notice = "You are blocked by the admin and cannot send messages.\n";
                sendAll(fromSock, notice.c_str(), notice.size());
            }
            return;
        }
        for (auto &c : clients)
        {
            if (c.name == toUser)
            {
                string formatted = "[PM from " + fromUser + "] " + msg + "\n";
                sendAll(c.socket, formatted.c_str(), formatted.size());
                return;
            }
        }
        if (fromSock != -1)
        {
            string notice = "User '" + toUser + "' is not online.\n";
            sendAll(fromSock, notice.c_str(), notice.size());
        }
    }

    void broadcastMessage(const string &message, int senderSocket, const string &room)
    {
        // Snapshot recipient fds under the lock so we don't hold the mutex
        // across send() — a slow consumer would otherwise stall the server.
        vector<int> recipients;
        {
            lock_guard<mutex> lock(clientsMutex);
            recipients.reserve(clients.size());
            for (auto &c : clients)
            {
                // An empty room means "all rooms" (admin broadcasts).
                if ((room.empty() || c.room == room) && c.socket != senderSocket)
                    recipients.push_back(c.socket);
            }
        }

        string framed = message + "\n";
        for (int fd : recipients)
        {
            if (!sendAll(fd, framed.c_str(), framed.size()))
            {
                // Owner thread (handleClient) sees EOF and removes the entry.
                shutdown(fd, SHUT_RDWR);
            }
        }
    }

    void removeClient(int clientSocket)
    {
        lock_guard<mutex> lock(clientsMutex);
        clients.erase(remove_if(clients.begin(), clients.end(),
                                [clientSocket](const ClientInfo &c)
                                { return c.socket == clientSocket; }),
                      clients.end());
    }

    // Execute an admin command (kick/say/slowmode) received over the socket and
    // reply to the requester. Shares behaviour with the stdin admin console.
    //
    // Authentication: when VECTORCOM_ADMIN_TOKEN is set, the wire form is
    //   /admin <token> <command...>
    // and the leading token must match. When it is empty, admin-over-socket
    // is refused entirely. The stdin console is always trusted because it is
    // local to the operator running the process.
    void handleAdminCommand(const string &fullCmd, int requesterSocket, const string &requester)
    {
        auto reply = [&](const string &s) {
            string m = s + "\n";
            sendAll(requesterSocket, m.c_str(), m.size());
        };

        if (adminToken.empty())
        {
            reply("Admin over socket is disabled on this server.");
            cout << "[admin DENY: disabled] from " << requester << endl;
            return;
        }

        // Strip the leading token. We deliberately don't echo the token or any
        // hint about its length in the reply or the log.
        size_t sp = fullCmd.find(' ');
        if (sp == string::npos)
        {
            reply("Admin command requires authentication.");
            cout << "[admin DENY: missing token] from " << requester << endl;
            return;
        }
        string presented = fullCmd.substr(0, sp);
        if (presented != adminToken)
        {
            reply("Admin authentication failed.");
            cout << "[admin DENY: bad token] from " << requester << endl;
            return;
        }
        string cmd = fullCmd.substr(sp + 1);

        if (cmd.rfind("kick ", 0) == 0)
        {
            string user = cmd.substr(5);
            kickUser(user);
            reply("[SERVER] kick requested: " + user);
        }
        else if (cmd.rfind("say ", 0) == 0)
        {
            string notice = "[SERVER] " + cmd.substr(4);
            broadcastMessage(notice, -1, "");
            reply("[SERVER] broadcast sent to all rooms");
        }
        else if (cmd.rfind("block ", 0) == 0)
        {
            string user = cmd.substr(6);
            {
                lock_guard<mutex> lock(clientsMutex);
                blockedUsers.insert(user);
            }
            notifyUser(user, "[SERVER] You have been blocked by the admin.");
            reply("[SERVER] blocked: " + user);
        }
        else if (cmd.rfind("unblock ", 0) == 0)
        {
            string user = cmd.substr(8);
            {
                lock_guard<mutex> lock(clientsMutex);
                blockedUsers.erase(user);
            }
            notifyUser(user, "[SERVER] You have been unblocked by the admin.");
            reply("[SERVER] unblocked: " + user);
        }
        else if (cmd.rfind("slowmode ", 0) == 0)
        {
            istringstream iss(cmd.substr(9));
            string room;
            int seconds = 0;
            if (iss >> room >> seconds)
            {
                {
                    lock_guard<mutex> lock(clientsMutex);
                    roomSlowmodeSeconds[room] = max(0, seconds);
                }
                string notice = "[SERVER] Slowmode for room '" + room + "' set to " + to_string(seconds) + "s";
                broadcastMessage(notice, -1, room);
                reply(notice);
            }
            else
            {
                reply("Usage: slowmode <room> <seconds>");
            }
        }
        else
        {
            reply("Unknown admin command. Try: kick <user> | say <msg> | block <user> | unblock <user> | slowmode <room> <secs>");
        }
        cout << "[admin via " << requester << "] " << cmd << endl;
    }

    void notifyUser(const string &username, const string &message)
    {
        lock_guard<mutex> lock(clientsMutex);
        for (auto &c : clients)
        {
            if (c.name == username)
            {
                string m = message + "\n";
                sendAll(c.socket, m.c_str(), m.size());
                return;
            }
        }
    }

    void kickUser(const string &username)
    {
        int targetFd = -1;
        {
            lock_guard<mutex> lock(clientsMutex);
            for (auto &c : clients)
            {
                if (c.name == username)
                {
                    targetFd = c.socket;
                    break;
                }
            }
        }
        if (targetFd < 0)
        {
            cout << "No such user: " << username << endl;
            return;
        }
        // Encrypt the kick notice (every other server frame is XOR-encrypted;
        // a plaintext send would arrive as garbage on the client).
        string msg = "[SERVER] You have been kicked by admin.\n";
        sendAll(targetFd, msg.c_str(), msg.size());
        // Don't close()/erase here: the owning handleClient thread may still
        // be in recv() on this fd, and a concurrent close would risk a UAF if
        // the kernel reuses the descriptor. shutdown() makes recv return 0;
        // the owner closes and removes the entry.
        shutdown(targetFd, SHUT_RDWR);
        cout << "Kicked user: " << username << endl;
    }
};

ChatServer *serverInstance = nullptr;
void signalHandler(int signal)
{
    if (signal == SIGINT || signal == SIGTERM)
    {
        cout << "\nShutting down server..." << endl;
        if (serverInstance)
            serverInstance->stop();
        exit(0);
    }
}

int main(int argc, char *argv[])
{
    int port = 8080;
    if (argc > 1)
    {
        try {
            port = stoi(argv[1]);
            if (port < 1 || port > 65535)
            {
                cerr << "Error: Port must be between 1 and 65535" << endl;
                return 1;
            }
        } catch (const exception &e) {
            cerr << "Error: Invalid port number" << endl;
            return 1;
        }
    }
    try
    {
        ChatServer server(port);
        serverInstance = &server;
        signal(SIGINT, signalHandler);
        signal(SIGTERM, signalHandler);
        signal(SIGPIPE, SIG_IGN);
        server.start();
    }
    catch (const exception &e)
    {
        cerr << "Error: " << e.what() << endl;
        return 1;
    }
    return 0;
}