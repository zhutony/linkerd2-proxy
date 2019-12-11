## Problems

We've received reports of issues where the proxy seems to stop receiving
endpoint updates for its services. As we debugged a proxy in this state, it
became clear that the proxy's client to the destination service had exhausted
its HTTP/2 connection window and was unable to receive new data. We validated
this by introducing requests into the proxy that would require a fresh
resolution and it became clear (from logs) that the resolution was not
satisfied and that the client had no connection window.

In a fresh proxy, we were able to reproduce a similar situation by:
1. Resolving _blue_, with an associated service profile;
2. Resolving _green_;
3. Triggering many service profile updates for _blue_;
4. Roll the pods in the _green_ service;
5. The proxy does not receive updates (due to an exhausted connection window).

When requests are not sent to _blue_ but updates are received, they end up
buffered and the proxy exerts backpressure; but the protocol-default window
configurations are such that a single stalled stream can exhaust the entire
connection window (for all requests)!

But, actually, this Should all be just fine: all dynamically-created services
should be dropped if they don't receive traffic for 1 minute (and once the
service is dropped, its metrics should be reaped ~9 minutes after that). When
the service is dropped, any pending resolution should also be dropped. Or, if
the service is not dropped, then it must be receiving traffic; and if it's
receiving traffic, then it must be polling for profile updates and can't be
stuck in this position!

Even when we remove all application load from the proxy, discovery updates
remain blocked on connection window. And furthermore, metrics _never_ seem to
be evicted for these services, which strongly points to these services being
leaked into the executor in some way.

## Remediation

Even though some questions remain about the nature of the problem, we can
implement a number of changes that will reduce the likelihood of this
behavior in the future; and we can instrument additional diagnostics to help
us identify instances of this bug.

### Eagerly read profiles from the router, regardless of traffic

https://github.com/linkerd/linkerd2-proxy/pull/397

### Increase the default connection window for all http/2 connections

The proxy's default connection window (65K is far too small for practical
use), as exhibited by this bug.

### Audit spawned services

Any time we move a Service into the executor (via `tokio::spawn`), we have
the potential to leak that service. If the task does not drop the service and
the task itself does not complete (i.e. so that it's dropped by the
executor), the service will be leaked. This is especially problematic in the
context of the proxy, as we bind resources (i.e. metrics, service discovery)
to the lifetime of the services that use those resources.

#### Router Purge task

https://github.com/linkerd/linkerd2-proxy/pull/396 fixes a bug where the
router's "purge" task--which holds references to the router's internal
services--would _never_ complete.

This bug _shouldn't_ cause the behavior we've observed, since
this should not cause evictions to stop. The task should evict all of its
inner services and then just stay around in the executor forever.

#### `tower::buffer::Worker` task

Every router in the proxy has a buffer inside of it. This is necessary
because the services within a router must be ready to serve requests
immediately (i.e. it must not be NotReady); so we need a buffer to queue
requests while the inner service is initialized.

So, is it possible for the buffer to leak its spawned worker? Turns out, yes;
but it's sort of by design:

`buffer::Worker` holds the inner `Service` and an `mpsc::Receiver` of
requests. As the worker task polled, requests are taken from the mpsc and
dispatched to the service. However, this must take into account the inner
service's readiness. If the inner service isn't ready, we're stuck. We can't
process any more requests or determine whether the mpsc's senders have
closed. But even if we could detect that the Buffer's senders have been
dropped, what would we do if we still had requests in the queue to be
processed? Really, they should be processed to completion.

So, what buffers in the proxy wrap a service that could get stuck in an
unready state indefinitely?

#### Balancer exhaustion vs Buffers

The service-discovery-informed load balancing layer is the prime suspect. It
seems almost obvious that if we can't receive any resolutions on the
controller client due to connection window underflow, then the load balancer
will get into this state if (1) it is new and cannot receive the initial
resolution or (2) if all of its endpoints have failed, i.e. because the
target cluster has changed.

So, if the balancer becomes stuck and empty, we'd expect its nearest buffer
worker to get stuck as well. But, we seem to be leaking upstream services as
well (i.e. those in the profile stack, above the balancer). We might
reasonably expect that these would be dropped when the router evicts the
top-level buffer. This isn't so.

When the balancer becomes unresponsive, the buffer that wraps it
_inevitablity_ becomes full, even in the face of request cancelation--because
request cancelatiion doesn't actually create more buffer capacity eagerly.
When this buffer fills, it becomes unready to upstream services. This
inevitably means that an upstream buffer's inner service starts returning
unready (because it contains a buffer that's full), and the buffer exhaustion
propagates up to the router, at which point we'll start rejecting request
eagerly due to load shedding.

We fill all the buffers. The route will never recover if the balancer does
not become ready; and it will never become ready unless the stuck connection
is dropped for some reason. This matches loads of anecdotal error reports.

#### So what are we gonna do about it?

- timeout balancer unreadiness (not great for apps; good for having a clear, predictable failure mode)
- cancelation strategy improvements?
- metrics
