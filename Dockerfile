FROM alpine:latest

RUN apk add --no-cache bash curl jq openssh-client

COPY ./entrypoint.sh /entrypoint.sh
COPY ./luks-ssh-unlock.sh /luks-ssh-unlock.sh

ENV SSH_HOST=example.com \
    SSH_KEY=/run/secrets/ssh_key \
    SSH_PORT=22 \
    SSH_JUMPHOST= \
    SSH_JUMPHOST_USERNAME= \
    SSH_JUMPHOST_PORT= \
    SSH_JUMPHOST_KEY= \
    SSH_USER=root \
    LUKS_PASSWORD= \
    LUKS_PASSWORD_FILE= \
    LUKS_TYPE=direct \
    EVENTS_FILE= \
    SLEEP_INTERVAL=5 \
    HEALTHCHECK_PORT= \
    HEALTHCHECK_REMOTE_HOSTNAME= \
    HEALTHCHECK_REMOTE_CMD= \
    APPRISE_URL= \
    APPRISE_TAG= \
    APPRISE_TITLE=

VOLUME /data/events

ENTRYPOINT ["/entrypoint.sh"]
