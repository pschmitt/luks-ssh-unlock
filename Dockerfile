FROM alpine:latest

RUN apk add --no-cache bash openssh-client

COPY ./entrypoint.sh /entrypoint.sh
COPY ./luks-unlock.sh /luks-unlock.sh

ENTRYPOINT ["/entrypoint.sh"]
