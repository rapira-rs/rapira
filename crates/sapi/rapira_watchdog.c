#include "rapira_sapi.h"
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>

#if PHP_VERSION_ID >= 80700
#include <stdatomic.h>
typedef atomic_bool rapira_interrupt_flag;
#define rapira_store_flag atomic_store
#else
#include <Zend/zend_atomic.h>
typedef zend_atomic_bool rapira_interrupt_flag;
#define rapira_store_flag zend_atomic_bool_store_ex
#endif

typedef struct {
    rapira_interrupt_flag *timed_out;
    rapira_interrupt_flag *vm_interrupt;
    pthread_t thread;
} rapira_interrupt;

#ifdef SIGRTMAX
static void rapira_watchdog_wake(int signo) { (void)signo; }
#endif

int rapira_watchdog_install(void) {
#ifdef SIGRTMAX
    // Use the high end of the application signal range. Zend's timer uses SIGRTMIN. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_max_execution_timer.c#L58-L63
    struct sigaction current;
    if (sigaction(SIGRTMAX, NULL, &current) == -1) {
        return errno;
    }
    if (current.sa_handler != SIG_DFL) {
        return EBUSY;
    }
    // A caller can block a signal for sigwait() without installing a handler. https://pubs.opengroup.org/onlinepubs/9799919799/functions/sigwait.html
    sigset_t set, previous;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMAX);
    int err = pthread_sigmask(SIG_BLOCK, &set, &previous);
    if (err != 0) {
        return err;
    }
    if (sigismember(&previous, SIGRTMAX)) {
        return EBUSY;
    }

    // A blocking system call must be able to return EINTR.
    struct sigaction action = {0};
    action.sa_handler = rapira_watchdog_wake;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGRTMAX, &action, NULL) == -1) {
        return errno;
    }
#endif
    return 0;
}

// Only PHP workers may receive the signal for Zend's execution timer.
int rapira_watchdog_block_timer(void) {
    sigset_t set;
    sigemptyset(&set);
#ifdef RAPIRA_TIMER_SIGNAL
    sigaddset(&set, RAPIRA_TIMER_SIGNAL);
#endif
    return pthread_sigmask(SIG_BLOCK, &set, NULL);
}

rapira_interrupt rapira_watchdog_capture(void) {
    return (rapira_interrupt){&EG(timed_out), &EG(vm_interrupt),
                              pthread_self()};
}

void rapira_watchdog_fire(const rapira_interrupt *slot) {
    rapira_store_flag(slot->timed_out, true);
    rapira_store_flag(slot->vm_interrupt, true);
#ifdef SIGRTMAX
    pthread_kill(slot->thread, SIGRTMAX);
#endif
}

void rapira_watchdog_clear(const rapira_interrupt *slot) {
    // The VM also uses vm_interrupt for pending extension callbacks.
    // https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_vm_def.h#L10737-L10753
    rapira_store_flag(slot->timed_out, false);
#ifdef SIGRTMAX
    // The unit can end with its wake signal still pending on this thread.
    sigset_t set, saved;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMAX);
    pthread_sigmask(SIG_BLOCK, &set, &saved);
    struct timespec zero = {0};
    while (sigtimedwait(&set, NULL, &zero) == -1 && errno == EINTR) {
    }
    pthread_sigmask(SIG_SETMASK, &saved, NULL);
#endif
}
