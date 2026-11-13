FROM composer:2 AS deps
WORKDIR /app
COPY composer.json .
RUN composer install --no-interaction --no-progress --quiet

FROM php:8.4-cli
WORKDIR /app
COPY --from=deps /app/vendor vendor
COPY *.php ./
ENTRYPOINT ["php"]
CMD ["es_client.php"]
