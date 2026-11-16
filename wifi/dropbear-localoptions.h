/* Dropbear for the dongle's network debug bridge (Settings > Developer > Network debugging).
 * Server only; root logs in with an enrolled ed25519 key (meowdbg pair), never a password.
 * Local port forwarding stays (ssh -L to reach dongle-local services from the Mac). */
#define DSS_PRIV_FILENAME "/data/net/debug/dss_host_key"
#define RSA_PRIV_FILENAME "/data/net/debug/rsa_host_key"
#define ECDSA_PRIV_FILENAME "/data/net/debug/ecdsa_host_key"
#define ED25519_PRIV_FILENAME "/data/net/debug/ed25519_host_key"
#define DROPBEAR_RSA 0
#define DROPBEAR_ECDSA 0
#define DROPBEAR_ED25519 1
#define DROPBEAR_SK_KEYS 0
#define DROPBEAR_ECDH 0
#define DROPBEAR_DH_GROUP14_SHA256 0
#define DROPBEAR_SNTRUP761 0
#define DROPBEAR_MLKEM768 1
#define DROPBEAR_CURVE25519 1
#define DROPBEAR_AES256 0
#define DROPBEAR_X11FWD 0
#define DROPBEAR_SVR_REMOTETCPFWD 0
#define DROPBEAR_SVR_LOCALSTREAMFWD 0
#define DROPBEAR_SVR_REMOTESTREAMFWD 0
#define DROPBEAR_SVR_AGENTFWD 0
#define DROPBEAR_SVR_PASSWORD_AUTH 0
#define DROPBEAR_SVR_MULTIUSER 1 /* 0 needs a kernel without CONFIG_MULTIUSER */
#define DROPBEAR_SFTPSERVER 0
#define DO_MOTD 0
#define INETD_MODE 0
#define DROPBEAR_REEXEC 0
#define MAX_UNAUTH_CLIENTS 4
#define MAX_UNAUTH_PER_IP 2
#define MAX_AUTH_TRIES 3
#define DEFAULT_PATH "/bin:/sbin:/usr/bin:/usr/sbin"
#define DEFAULT_ROOT_PATH "/bin:/sbin:/usr/bin:/usr/sbin"
