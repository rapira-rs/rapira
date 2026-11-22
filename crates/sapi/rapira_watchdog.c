#include "rapira_sapi.h"
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <sys/time.h>
#include <time.h>

#if PHP_VERSION_ID >= 80700
#include <stdatomic.h>
typedef atomic_bool rapira_interrupt_flag;
#define rapira_load_flag atomic_load
#define rapira_store_flag atomic_store
#else
#include <Zend/zend_atomic.h>
typedef zend_atomic_bool rapira_interrupt_flag;
#define rapira_load_flag zend_atomic_bool_load_ex
#define rapira_store_flag zend_atomic_bool_store_ex
#endif

typedef struct {
    rapira_interrupt_flag *timed_out;
    rapira_interrupt_flag *vm_interrupt;
    pthread_t thread;
} rapira_interrupt;

#ifdef SIGRTMIN
// Wake the PHP thread with a separate signal so Zend's SIGRTMIN timer handler does not run. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_max_execution_timer.c#L58-L63
#define RAPIRA_WAKE_SIGNAL (SIGRTMIN + 3)

static void rapira_watchdog_wake(int signo) { (void)signo; }
#endif

int rapira_watchdog_install(void) {
#ifdef RAPIRA_WAKE_SIGNAL
    // A blocking system call must be able to return EINTR.
    struct sigaction action = {0};
    action.sa_handler = rapira_watchdog_wake;
    sigemptyset(&action.sa_mask);
    if (sigaction(RAPIRA_WAKE_SIGNAL, &action, NULL) == -1) {
        return errno;
    }
#endif
    return 0;
}

// Only PHP workers may receive the signal for Zend's execution timer.
// https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_execute_API.c#L1668-L1697
#ifdef ZEND_MAX_EXECUTION_TIMERS
#define RAPIRA_TIMER_SIGNAL SIGRTMIN
#elif defined(HAVE_SETITIMER)
#if defined(__CYGWIN__) || defined(__PASE__) ||                                \
    (defined(__aarch64__) && defined(__APPLE__))
#define RAPIRA_TIMER_SIGNAL SIGALRM
#define RAPIRA_ITIMER ITIMER_REAL
#else
#define RAPIRA_TIMER_SIGNAL SIGPROF
#define RAPIRA_ITIMER ITIMER_PROF
#endif
#endif

int rapira_watchdog_mask(int how, bool wake) {
    sigset_t set;
    sigemptyset(&set);
#ifdef RAPIRA_TIMER_SIGNAL
    sigaddset(&set, RAPIRA_TIMER_SIGNAL);
#endif
#ifdef RAPIRA_WAKE_SIGNAL
    if (wake) {
        sigaddset(&set, RAPIRA_WAKE_SIGNAL);
    }
#endif
    return pthread_sigmask(how, &set, NULL);
}

rapira_interrupt rapira_watchdog_capture(void) {
    return (rapira_interrupt){&EG(timed_out), &EG(vm_interrupt),
                              pthread_self()};
}

void rapira_watchdog_fire(const rapira_interrupt *slot) {
#if !defined(ZTS) && defined(RAPIRA_ITIMER)
    // NTS treats a timer signal with timed_out set as a hard timeout.
    // Stop its process timer if the watchdog expires first.
    // https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_execute_API.c#L1559-L1587
    if (!rapira_load_flag(slot->timed_out)) {
        struct itimerval off = {0};
        setitimer(RAPIRA_ITIMER, &off, NULL);
    }
#endif
    rapira_store_flag(slot->timed_out, true);
    rapira_store_flag(slot->vm_interrupt, true);
#ifdef RAPIRA_WAKE_SIGNAL
    pthread_kill(slot->thread, RAPIRA_WAKE_SIGNAL);
#endif
}

void rapira_watchdog_clear(const rapira_interrupt *slot) {
    // The VM also uses vm_interrupt for pending extension callbacks.
    // https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_vm_def.h#L10737-L10753
    rapira_store_flag(slot->timed_out, false);
#ifdef RAPIRA_WAKE_SIGNAL
    // The unit can end with its wake signal still pending on this thread.
    sigset_t set, saved;
    sigemptyset(&set);
    sigaddset(&set, RAPIRA_WAKE_SIGNAL);
    pthread_sigmask(SIG_BLOCK, &set, &saved);
    struct timespec zero = {0};
    while (sigtimedwait(&set, NULL, &zero) == -1 && errno == EINTR) {
    }
    pthread_sigmask(SIG_SETMASK, &saved, NULL);
#endif
}
