#include <arpa/inet.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <libssh/libssh.h>
#include <libssh/server.h>
#include <libssh/sftp.h>

static void require(bool condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "%s\n", message);
        exit(EXIT_FAILURE);
    }
}

static bool authenticate(ssh_session session, ssh_key client) {
    bool key_accepted = false;
    unsigned int round = 0;
    char account[64] = {0};
    ssh_message message;
    while ((message = ssh_message_get(session)) != NULL) {
        if (ssh_message_type(message) != SSH_REQUEST_AUTH) {
            ssh_message_reply_default(message);
            ssh_message_free(message);
            continue;
        }
        const char *provided_user = ssh_message_auth_user(message);
        if (provided_user != NULL) {
            require(strlen(provided_user) < sizeof(account), "Fixture authentication user is too long");
            snprintf(account, sizeof(account), "%s", provided_user);
        }
        // Keyboard-interactive response packets omit the user field.
        const char *user = account;
        require(user[0] != '\0', "Fixture did not receive an authentication user");
        require(strcmp(user, "unknown-host") != 0, "Authentication started before host verification");
        bool needs_key = strcmp(user, "mfa-fixture") == 0;
        int selected = ssh_message_subtype(message);
        int allowed = needs_key && !key_accepted ? SSH_AUTH_METHOD_PUBLICKEY : SSH_AUTH_METHOD_INTERACTIVE;
        ssh_message_auth_set_methods(message, allowed);
        ssh_set_auth_methods(session, allowed);
        if (selected == SSH_AUTH_METHOD_PUBLICKEY && allowed == SSH_AUTH_METHOD_PUBLICKEY &&
            ssh_key_cmp(ssh_message_auth_pubkey(message), client, SSH_KEY_CMP_PUBLIC) == 0) {
            if (ssh_message_auth_publickey_state(message) == SSH_PUBLICKEY_STATE_NONE) {
                require(ssh_message_auth_reply_pk_ok_simple(message) == SSH_OK, "Fixture could not offer key authentication");
            } else {
                require(ssh_message_auth_publickey_state(message) == SSH_PUBLICKEY_STATE_VALID, "Fixture key signature was invalid");
                key_accepted = true;
                ssh_set_auth_methods(session, SSH_AUTH_METHOD_INTERACTIVE);
                ssh_message_auth_set_methods(message, SSH_AUTH_METHOD_INTERACTIVE);
                require(ssh_message_auth_reply_success(message, 1) == SSH_OK, "Fixture could not send partial success");
            }
        } else if (selected == SSH_AUTH_METHOD_INTERACTIVE && allowed == SSH_AUTH_METHOD_INTERACTIVE) {
            if (!ssh_message_auth_kbdint_is_response(message)) {
                const char *prompts[] = {"One-time code", "Account label"};
                char echo[] = {0, 1};
                require(ssh_message_auth_interactive_request(message, "Fixture authentication", "First round\nTwo answers", 2, prompts, echo) == SSH_OK,
                        "Fixture could not send first prompts");
                round = 1;
            } else {
                require(strcmp(user, "cancel-fixture") != 0 && strcmp(user, "timeout-fixture") != 0,
                        "Fixture received an answer after an aborted prompt");
                if (strcmp(user, "reject-fixture") == 0) {
                    ssh_message_reply_default(message);
                } else if (round == 1) {
                    require(ssh_userauth_kbdint_getnanswers(session) == 2 &&
                            strcmp(ssh_userauth_kbdint_getanswer(session, 0), "fixture-code") == 0 &&
                            strcmp(ssh_userauth_kbdint_getanswer(session, 1), "fixture-label") == 0,
                            "Fixture received incorrect first answers");
                    const char *prompts[] = {"Confirmation"};
                    char echo[] = {0};
                    require(ssh_message_auth_interactive_request(message, "Fixture authentication", "Second round", 1, prompts, echo) == SSH_OK,
                            "Fixture could not send the second prompt");
                    round = 2;
                } else {
                    require(round == 2 && ssh_userauth_kbdint_getnanswers(session) == 1 &&
                            strcmp(ssh_userauth_kbdint_getanswer(session, 0), "fixture-confirmation") == 0,
                            "Fixture received an incorrect second answer");
                    require(ssh_message_auth_reply_success(message, 0) == SSH_OK, "Fixture could not authenticate the client");
                    ssh_message_free(message);
                    return true;
                }
            }
        } else {
            ssh_message_reply_default(message);
        }
        ssh_message_free(message);
    }
    return false;
}

static bool serve_directory(ssh_session session) {
    ssh_channel channel = NULL;
    ssh_message message;
    while ((message = ssh_message_get(session)) != NULL) {
        int type = ssh_message_type(message);
        int subtype = ssh_message_subtype(message);
        if (channel == NULL && type == SSH_REQUEST_CHANNEL_OPEN && subtype == SSH_CHANNEL_SESSION) {
            channel = ssh_message_channel_request_open_reply_accept(message);
        } else if (channel != NULL && type == SSH_REQUEST_CHANNEL && subtype == SSH_CHANNEL_REQUEST_SUBSYSTEM &&
                   strcmp(ssh_message_channel_request_subsystem(message), "sftp") == 0) {
            require(ssh_message_channel_request_reply_success(message) == SSH_OK, "Fixture could not open SFTP");
            ssh_message_free(message);
            break;
        } else {
            ssh_message_reply_default(message);
        }
        ssh_message_free(message);
    }
    if (channel == NULL) {
        return false;
    }
    sftp_session sftp = sftp_server_new(session, channel);
    require(sftp != NULL && sftp_server_init(sftp) == SSH_OK, "Fixture could not start SFTP");
    struct sftp_attributes_struct directory = {
        .flags = SSH_FILEXFER_ATTR_PERMISSIONS,
        .type = SSH_FILEXFER_TYPE_DIRECTORY,
        .permissions = S_IFDIR | 0700,
    };
    sftp_client_message request;
    while ((request = sftp_get_client_message(sftp)) != NULL) {
        switch (sftp_client_message_get_type(request)) {
        case SSH_FXP_REALPATH:
            sftp_reply_name(request, "/fixture", &directory);
            break;
        case SSH_FXP_STAT:
        case SSH_FXP_LSTAT:
            sftp_reply_attr(request, &directory);
            break;
        default:
            sftp_reply_status(request, SSH_FX_OP_UNSUPPORTED, "Unsupported fixture request");
            break;
        }
        sftp_client_message_free(request);
    }
    sftp_server_free(sftp);
    ssh_channel_free(channel);
    return true;
}

int main(int argc, char **argv) {
    require(argc == 3, "Expected a disposable host key and a client public key");
    ssh_bind server = ssh_bind_new();
    ssh_key host = NULL;
    ssh_key client = NULL;
    require(server != NULL && ssh_pki_import_privkey_file(argv[1], NULL, NULL, NULL, &host) == SSH_OK &&
            ssh_pki_import_pubkey_file(argv[2], &client) == SSH_OK, "Fixture could not load keys");
    bool use_config = false;
    require(ssh_bind_options_set(server, SSH_BIND_OPTIONS_PROCESS_CONFIG, &use_config) == SSH_OK &&
            ssh_bind_options_set(server, SSH_BIND_OPTIONS_IMPORT_KEY, host) == SSH_OK,
            "Fixture could not configure its test host key");

    int listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t length = sizeof(address);
    require(listener >= 0 && bind(listener, (struct sockaddr *)&address, length) == 0 &&
            listen(listener, 8) == 0 && getsockname(listener, (struct sockaddr *)&address, &length) == 0,
            "Fixture could not listen on loopback");
    printf("%u\n", (unsigned int)ntohs(address.sin_port));
    fflush(stdout);
    for (int attempt = 0; attempt < 6; ++attempt) {
        int fd = accept(listener, NULL, NULL);
        ssh_session session = ssh_new();
        require(fd >= 0 && session != NULL && ssh_bind_accept_fd(server, session, fd) == SSH_OK,
                "Fixture could not accept the client");
        require(ssh_handle_key_exchange(session) == SSH_OK, "Fixture could not exchange SSH keys");
        if (authenticate(session, client)) {
            while (serve_directory(session)) {
                // The client may close and reopen its SFTP channel.
            }
        }
        ssh_free(session);
    }
    close(listener);
    ssh_key_free(client);
    ssh_bind_free(server);
    return EXIT_SUCCESS;
}
