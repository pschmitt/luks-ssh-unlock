FROM alpine:latest

RUN apk add --no-cache bash curl jq openssh-client

COPY ./entrypoint.sh /entrypoint.sh
COPY ./luks-unlock.sh /luks-unlock.sh

ENV SSH_HOST=example.com \
    SSH_KEY=/run/secrets/ssh_key \
    SSH_PORT=22 \
    SSH_USER=root \
    LUKS_PASSWORD= \
    LUKS_PASSWORD_FILE= \
    LUKS_TYPE=direct \
    EVENTS_FILE= \
    SLEEP_INTERVAL=5 \
    HEALTHCHECK_PORT= \
    APPRISE_URL= \
    APPRISE_TAG= \
    APPRISE_TITLE

VOLUME /data/events

ENTRYPOINT ["/entrypoint.sh"]
