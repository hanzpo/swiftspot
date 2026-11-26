#ifndef SWIFTSPOT_H
#define SWIFTSPOT_H

#include <stddef.h>
#include <stdint.h>

typedef struct SwiftSpotEngine SwiftSpotEngine;

typedef void (*swiftspot_deliver)(void *context, const uint8_t *data, size_t len);

SwiftSpotEngine *swiftspot_start(swiftspot_deliver deliver, void *context);
void swiftspot_send(const SwiftSpotEngine *engine, const uint8_t *data, size_t len);
void swiftspot_stop(SwiftSpotEngine *engine);

#endif
