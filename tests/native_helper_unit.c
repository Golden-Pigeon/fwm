/* Exercise the shipped Linux helper's actual parsers without opening procfs,
 * connecting to SSH, or sending signals. Never run its self-removing main. */
#define main native_helper_program_main
#include "../crates/fwm-core/src/cleanup/native_helper.c"
#undef main

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "%s:%d: %s\n", __func__, __LINE__, #condition); \
        return 1; \
    } \
} while (0)

#define TEST_BOOT "9e37a5c5-f6c1-443e-9491-33eaad32c6c9"
#define TEST_OWNER "11111111-1111-4111-8111-111111111111"
#define TEST_RULE "22222222-2222-4222-8222-222222222222"
#define TEST_SESSION "33333333-3333-4333-8333-333333333333"
#define TEST_INODE 4294967311UL

static struct identity expected_identity(void) {
    struct identity id = {0};
    id.pid = 2222;
    id.uid = 1000;
    id.start = 9876543;
    strcpy(id.boot, TEST_BOOT);
    strcpy(id.name, "sshd");
    return id;
}

static int test_tcp4_rows(void) {
    /* /proc/net/tcp's documented field layout: UID follows tx:rx, timer,
     * and retransmits; inode follows UID and timeout. Trailing fields vary. */
    const char *established =
        "   7: 0100007F:0016 0200007F:CEAA 01 "
        "00000000:00000000 02:0000157F 00000000 "
        "1000 0 4294967311 2 0000000000000000 20 4 30 10 -1\n";
    struct tcp4_row row;
    CHECK(parse_tcp4_row(established, &row) == 0);
    char local[INET_ADDRSTRLEN], remote[INET_ADDRSTRLEN];
    CHECK(inet_ntop(AF_INET, &row.local, local, sizeof(local)) != NULL);
    CHECK(inet_ntop(AF_INET, &row.remote, remote, sizeof(remote)) != NULL);
    CHECK(strcmp(local, "127.0.0.1") == 0);
    CHECK(strcmp(remote, "127.0.0.2") == 0);
    CHECK(row.local_port == 22);
    CHECK(row.remote_port == 52906);
    CHECK(row.state == 1);
    CHECK(row.uid == 1000);
    CHECK(row.inode == TEST_INODE);

    const char *listener =
        "0: 0100007F:1ED2 00000000:0000 0A "
        "00000000:00000000 00:00000000 00000000 1000 0 4294967312\n";
    CHECK(parse_tcp4_row(listener, &row) == 0);
    CHECK(row.local_port == 7890 && row.remote_port == 0);
    CHECK(row.state == 0x0A && row.uid == 1000);
    CHECK(row.inode == 4294967312UL);
    CHECK(parse_tcp4_row("sl local_address rem_address st tx_queue rx_queue", &row) < 0);
    CHECK(parse_tcp4_row("0: 0100007F:0016 0200007F:CEAA 01", &row) < 0);
    CHECK(parse_tcp4_row("", &row) < 0);
    return 0;
}

static int test_ssh_connection(void) {
    struct transport transport;
    CHECK(setenv("SSH_CONNECTION", "127.0.0.2 52906 127.0.0.1 22", 1) == 0);
    CHECK(read_transport(&transport) == 0);
    CHECK(strcmp(transport.client, "127.0.0.2") == 0);
    CHECK(strcmp(transport.server, "127.0.0.1") == 0);
    CHECK(transport.client_port == 52906 && transport.server_port == 22);
    const char *invalid[] = {
        "127.0.0.2 52906 127.0.0.1 22 extra",
        "127.0.0.2 52906 127.0.0.1",
        "127.0.0.2 0 127.0.0.1 22",
        "127.0.0.2 52906 127.0.0.1 65536",
        "127.0.0.2 52906 127.0.0.1 22junk",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 52906 127.0.0.1 22",
        "127.0.0.2 52906 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 22",
    };
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
        CHECK(setenv("SSH_CONNECTION", invalid[i], 1) == 0);
        CHECK(read_transport(&transport) < 0);
    }
    CHECK(setenv("SSH_CONNECTION", "::1 52906 ::1 22", 1) == 0);
    CHECK(read_transport(&transport) == -2);
    CHECK(unsetenv("SSH_CONNECTION") == 0);
    CHECK(read_transport(&transport) < 0);
    return 0;
}

static int test_native_identity(void) {
    const char *record =
        "{\"session_pid\":2222,\"session_uid\":1000,\"session_start\":9876543,"
        "\"session_birth\":\"" TEST_BOOT ":9876543\",\"session_name\":\"sshd\"}";
    struct identity parsed = {0}, expected = expected_identity();
    pid_t pid;
    CHECK(old_identity(record, &parsed, &pid) == 0);
    CHECK(pid == expected.pid && same_id(&parsed, &expected));
    return 0;
}

static int test_python_identity(void) {
    const char *record =
        "{\"session\":{\"pid\":2222,\"ppid\":1111,\"uid\":1000,"
        "\"birth\":\"" TEST_BOOT ":9876543\",\"name\":\"sshd\",\"state\":\"S\"}}";
    struct identity parsed = {0}, expected = expected_identity();
    pid_t pid;
    CHECK(old_identity(record, &parsed, &pid) == 0);
    CHECK(pid == expected.pid && same_id(&parsed, &expected));
    return 0;
}

static int test_invalid_identity(void) {
    const char *invalid[] = {
        "{\"session_pid\":2222,\"session_uid\":1000,\"session_start\":9876543,"
        "\"session_birth\":\"" TEST_BOOT ":9876544\",\"session_name\":\"sshd\"}",
        "{\"session_pid\":2222,\"session_uid\":1000,\"session_start\":9876543,"
        "\"session_birth\":\"" TEST_BOOT ":9876543junk\",\"session_name\":\"sshd\"}",
        "{\"session\":{\"pid\":2222,\"uid\":1000,\"birth\":\"" TEST_BOOT ":9876543junk\",\"name\":\"sshd\"}}",
        "{\"session\":{\"pid\":2222,\"uid\":1000,\"birth\":\"" TEST_BOOT ":-1\",\"name\":\"sshd\"}}",
        "{\"session\":{\"pid\":2222,\"uid\":1000,\"birth\":\"" TEST_BOOT ":\",\"name\":\"sshd\"}}",
        "{\"session\":{\"pid\":2222,\"uid\":1000,\"birth\":\":9876543\",\"name\":\"sshd\"}}",
    };
    struct identity parsed;
    pid_t pid;
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
        CHECK(old_identity(invalid[i], &parsed, &pid) < 0);
    }
    return 0;
}

static int test_claim_record_roundtrip(void) {
    struct helper_state state = {0};
    strcpy(state.current.owner, TEST_OWNER);
    strcpy(state.current.rule, TEST_RULE);
    strcpy(state.current.session, TEST_SESSION);
    strcpy(state.current.host, "127.0.0.1");
    state.current.port = 7890;
    state.current.generation = 9;
    state.session = expected_identity();
    state.helper = expected_identity();
    state.helper.pid = 2223;
    strcpy(state.helper.name, "fwm-helper");
    strcpy(state.transport.client, "127.0.0.2");
    strcpy(state.transport.server, "127.0.0.1");
    state.transport.client_port = 52906;
    state.transport.server_port = 22;
    state.transport_uid = 0; /* sshd's accepted transport can retain root UID. */
    state.transport_inode = TEST_INODE;
    char record[MAX_LINE];
    CHECK(make_record(&state, record, sizeof(record), "claimed") == 0);
    struct identity parsed;
    pid_t pid;
    CHECK(old_identity(record, &parsed, &pid) == 0);
    CHECK(same_id(&parsed, &state.session));
    unsigned long long nested_pid;
    CHECK(json_nested_u64(record, "session", "pid", &nested_pid) == 0 && nested_pid == 2222);
    CHECK(json_nested_u64(record, "helper", "pid", &nested_pid) == 0 && nested_pid == 2223);
    CHECK(match_current(record, &state.current));
    CHECK(session_proof_matches(record, 0, TEST_INODE));
    struct transport parsed_transport;
    CHECK(old_transport(record, &parsed_transport) == 0);
    CHECK(strcmp(parsed_transport.client, state.transport.client) == 0);
    CHECK(strcmp(parsed_transport.server, state.transport.server) == 0);
    CHECK(parsed_transport.client_port == state.transport.client_port);
    CHECK(parsed_transport.server_port == state.transport.server_port);
    state.listener_uid = 1000;
    state.listener_inode = TEST_INODE + 1;
    CHECK(make_record(&state, record, sizeof(record), "confirmed") == 0);
    CHECK(listener_proof_matches(record, 1000, TEST_INODE + 1));
    return 0;
}

static int test_flat_proofs(void) {
    const char *record =
        "{\"session_proof\":{\"source\":\"ssh_exec_ancestry_inode\",\"uid\":0,\"inode\":4294967311},"
        "\"listener_proof\":{\"source\":\"ssh_forward_ack_inode\",\"uid\":1000,\"inode\":4294967312}}";
    CHECK(session_proof_matches(record, 0, TEST_INODE));
    CHECK(!session_proof_matches(record, 1000, TEST_INODE));
    CHECK(!session_proof_matches(record, 0, TEST_INODE + 1));
    CHECK(listener_proof_matches(record, 1000, TEST_INODE + 1));
    CHECK(!listener_proof_matches(record, 0, TEST_INODE + 1));
    CHECK(!listener_proof_matches(record, 1000, TEST_INODE));
    return 0;
}

static int test_python_proofs(void) {
    const char *record =
        "{\"session_proof\":{\"source\":\"ssh_exec_ancestry_inode\",\"socket\":{"
        "\"local\":[\"127.0.0.1\",22],\"remote\":[\"127.0.0.2\",52906],"
        "\"listening\":false,\"inode\":\"4294967311\",\"uid\":0}},"
        "\"listener_proof\":{\"source\":\"ssh_forward_ack_inode\",\"sockets\":[{"
        "\"local\":[\"127.0.0.1\",7890],\"remote\":[\"0.0.0.0\",0],"
        "\"listening\":true,\"inode\":\"4294967312\",\"uid\":1000}]}}";
    CHECK(session_proof_matches(record, 0, TEST_INODE));
    CHECK(!session_proof_matches(record, 0, TEST_INODE + 1));
    CHECK(listener_proof_matches(record, 1000, TEST_INODE + 1));
    CHECK(!listener_proof_matches(record, 1000, TEST_INODE));
    return 0;
}

static int test_invalid_proofs(void) {
    const char *invalid[] = {
        "{}",
        "{\"uid\":1000}",
        "{\"inode\":4294967311}",
        "{\"uid\":1000,\"inode\":0}",
        "{\"uid\":1000,\"inode\":-1}",
        "{\"uid\":-1,\"inode\":4294967311}",
        "{\"uid\":1000,\"inode\":4294967311junk}",
        "{\"uid\":1000,\"inode\":\"4294967311junk\"}",
        "{\"uid\":1000,\"inode\":\"\"}",
        "{\"uid\":1000,\"inode\":18446744073709551616}",
    };
    char record[1024];
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
        CHECK(snprintf(record, sizeof(record),
            "{\"session_proof\":{\"source\":\"process_fd\"%s%s,"
            "\"listener_proof\":{\"source\":\"process_fd\"%s%s}",
            strlen(invalid[i]) > 2 ? "," : "", invalid[i] + 1,
            strlen(invalid[i]) > 2 ? "," : "", invalid[i] + 1) > 0);
        CHECK(!session_proof_matches(record, 1000, TEST_INODE));
        CHECK(!listener_proof_matches(record, 1000, TEST_INODE));
    }
    const char *outside =
        "{\"session_proof\":{\"source\":\"process_fd\"},\"listener_proof\":{\"source\":\"process_fd\"},"
        "\"unrelated\":{\"uid\":1000,\"inode\":4294967311}}";
    CHECK(!session_proof_matches(outside, 1000, TEST_INODE));
    CHECK(!listener_proof_matches(outside, 1000, TEST_INODE));
    const char *zero =
        "{\"session_proof\":{\"source\":\"process_fd\",\"uid\":1000,\"inode\":0},"
        "\"listener_proof\":{\"source\":\"process_fd\",\"uid\":1000,\"inode\":0}}";
    CHECK(!session_proof_matches(zero, 1000, 0));
    CHECK(!listener_proof_matches(zero, 1000, 0));
    return 0;
}

static int test_registry_paths(void) {
    const char *test_directory = getenv("FWM_TEST_DIRECTORY");
    CHECK(test_directory && test_directory[0] == '/');
    char xdg[PATH_CAP], override[PATH_CAP], expected[PATH_CAP];
    CHECK(snprintf(xdg, sizeof(xdg), "%s/xdg", test_directory) < (int)sizeof(xdg));
    CHECK(snprintf(override, sizeof(override), "%s/override", test_directory) < (int)sizeof(override));
    CHECK(setenv("XDG_STATE_HOME", xdg, 1) == 0);
    CHECK(unsetenv("FWM_REMOTE_STATE_DIR") == 0);
    struct helper_state state = {0};
    state.lock_fd = -1;
    CHECK(registry(&state, TEST_OWNER, TEST_RULE) == 0);
    unlock_registry(&state);
    CHECK(snprintf(expected, sizeof(expected), "%s/fwm/leases/%s/%s.json", xdg,
                   TEST_OWNER, TEST_RULE) < (int)sizeof(expected));
    CHECK(strcmp(state.path, expected) == 0);
    struct stat directory;
    CHECK(lstat(state.directory, &directory) == 0);
    CHECK(directory.st_uid == geteuid() && !(directory.st_mode & 077));

    CHECK(setenv("FWM_REMOTE_STATE_DIR", override, 1) == 0);
    CHECK(registry(&state, TEST_OWNER, TEST_RULE) == 0);
    unlock_registry(&state);
    CHECK(snprintf(expected, sizeof(expected), "%s/leases/%s/%s.json", override,
                   TEST_OWNER, TEST_RULE) < (int)sizeof(expected));
    CHECK(strcmp(state.path, expected) == 0);

    CHECK(unsetenv("FWM_REMOTE_STATE_DIR") == 0);
    CHECK(setenv("XDG_STATE_HOME", "relative-state", 1) == 0);
    CHECK(registry(&state, TEST_OWNER, TEST_RULE) < 0);
    unlock_registry(&state);
    return 0;
}

int main(int argc, char **argv) {
    struct test_case { const char *name; int (*run)(void); } cases[] = {
        {"tcp4_rows", test_tcp4_rows},
        {"ssh_connection", test_ssh_connection},
        {"native_identity", test_native_identity},
        {"python_identity", test_python_identity},
        {"invalid_identity", test_invalid_identity},
        {"claim_record_roundtrip", test_claim_record_roundtrip},
        {"flat_proofs", test_flat_proofs},
        {"python_proofs", test_python_proofs},
        {"invalid_proofs", test_invalid_proofs},
        {"registry_paths", test_registry_paths},
    };
    if (argc != 2) {
        fprintf(stderr, "usage: %s TEST_CASE\n", argv[0]);
        return 2;
    }
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        if (!strcmp(argv[1], cases[i].name)) return cases[i].run();
    }
    fprintf(stderr, "unknown test case: %s\n", argv[1]);
    return 2;
}
