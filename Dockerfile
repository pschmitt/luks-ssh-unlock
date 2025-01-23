# hadolint ignore=DL3007
FROM alpine:latest

# hadolint ignore=DL3018
RUN apk add --no-cache bash curl bind-tools jq msmtp openssh-client && \
  ln -sfv /usr/bin/msmtp /usr/sbin/sendmail

COPY ./entrypoint.sh /entrypoint.sh
COPY ./luks-ssh-unlock.sh /luks-ssh-unlock.sh

ENV SSH_HOST=example.com \
    SSH_KEY=/run/secrets/ssh_key \
    SSH_PORT=22 \
    FORCE_IPV4= \
    FORCE_IPV6= \
    SSH_JUMPHOST= \
    SSH_JUMPHOST_USERNAME= \
    SSH_JUMPHOST_PORT= \
    SSH_JUMPHOST_KEY= \
    SSH_USER=root \
    LUKS_PASSPHRASE= \
    LUKS_PASSPHRASE_FILE= \
    LUKS_TYPE=raw \
    EVENTS_FILE= \
    SLEEP_INTERVAL=5 \
    HEALTHCHECK_PORT= \
    HEALTHCHECK_REMOTE_HOSTNAME= \
    HEALTHCHECK_REMOTE_USERNAME= \
    HEALTHCHECK_REMOTE_CMD= \
    APPRISE_URL= \
    APPRISE_TAG= \
    APPRISE_TITLE= \
    EMAIL_FROM= \
    EMAIL_RECIPIENT= \
    EMAIL_SUBJECT= \
    MSMTPRC=/config/msmtprc

VOLUME /config
VOLUME /data/events

ENTRYPOINT ["/entrypoint.sh"]
