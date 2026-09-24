# Architecture

Checkout publishes `orders.created` after a successful `PlaceOrder`.

## Billing

Billing consumes `orders.created` and records the invoice.

## Notify

Notify consumes `orders.created` and sends the confirmation email.
