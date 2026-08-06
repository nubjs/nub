// Minimal EndpointSecurity client. The ONLY question it answers: what does es_new_client()
// return in this process's signing/SIP state? The result enum distinguishes "missing
// entitlement" (AMFI) from "missing TCC Full Disk Access" (NOT_PERMITTED) from success,
// which is exactly the fork in the recommendation.
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <EndpointSecurity/EndpointSecurity.h>
#include <bsm/libbsm.h>

static const char *name_of(es_new_client_result_t r) {
  switch (r) {
    case ES_NEW_CLIENT_RESULT_SUCCESS: return "SUCCESS";
    case ES_NEW_CLIENT_RESULT_ERR_INVALID_ARGUMENT: return "ERR_INVALID_ARGUMENT";
    case ES_NEW_CLIENT_RESULT_ERR_INTERNAL: return "ERR_INTERNAL";
    case ES_NEW_CLIENT_RESULT_ERR_NOT_ENTITLED: return "ERR_NOT_ENTITLED";
    case ES_NEW_CLIENT_RESULT_ERR_NOT_PERMITTED: return "ERR_NOT_PERMITTED";
    case ES_NEW_CLIENT_RESULT_ERR_NOT_PRIVILEGED: return "ERR_NOT_PRIVILEGED";
    case ES_NEW_CLIENT_RESULT_ERR_TOO_MANY_CLIENTS: return "ERR_TOO_MANY_CLIENTS";
    default: return "UNKNOWN";
  }
}

int main(int argc, char **argv) {
  int seconds = argc > 1 ? atoi(argv[1]) : 0;
  const char *filter = argc > 2 ? argv[2] : NULL;

  es_client_t *client = NULL;
  es_new_client_result_t res = es_new_client(&client, ^(es_client_t *c, const es_message_t *msg) {
    (void)c;
    const char *ename = "?";
    char a[4096] = "", b[4096] = "";
    switch (msg->event_type) {
      case ES_EVENT_TYPE_NOTIFY_CREATE:
        ename = "CREATE";
        if (msg->event.create.destination_type == ES_DESTINATION_TYPE_NEW_PATH)
          snprintf(a, sizeof(a), "%s/%s",
                   msg->event.create.destination.new_path.dir->path.data,
                   msg->event.create.destination.new_path.filename.data);
        else
          snprintf(a, sizeof(a), "%s", msg->event.create.destination.existing_file->path.data);
        break;
      case ES_EVENT_TYPE_NOTIFY_WRITE:
        ename = "WRITE"; snprintf(a, sizeof(a), "%s", msg->event.write.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_UNLINK:
        ename = "UNLINK"; snprintf(a, sizeof(a), "%s", msg->event.unlink.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_RENAME:
        ename = "RENAME";
        snprintf(a, sizeof(a), "%s", msg->event.rename.source->path.data);
        if (msg->event.rename.destination_type == ES_DESTINATION_TYPE_NEW_PATH)
          snprintf(b, sizeof(b), "%s/%s",
                   msg->event.rename.destination.new_path.dir->path.data,
                   msg->event.rename.destination.new_path.filename.data);
        else
          snprintf(b, sizeof(b), "%s", msg->event.rename.destination.existing_file->path.data);
        break;
      case ES_EVENT_TYPE_NOTIFY_LINK:
        ename = "LINK";
        snprintf(a, sizeof(a), "%s", msg->event.link.source->path.data);
        snprintf(b, sizeof(b), "%s/%s", msg->event.link.target_dir->path.data,
                 msg->event.link.target_filename.data);
        break;
      case ES_EVENT_TYPE_NOTIFY_CLONE:
        ename = "CLONE";
        snprintf(a, sizeof(a), "%s", msg->event.clone.source->path.data);
        snprintf(b, sizeof(b), "%s/%s", msg->event.clone.target_dir->path.data,
                 msg->event.clone.target_name.data);
        break;
      case ES_EVENT_TYPE_NOTIFY_TRUNCATE:
        ename = "TRUNCATE"; snprintf(a, sizeof(a), "%s", msg->event.truncate.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_SETMODE:
        ename = "SETMODE"; snprintf(a, sizeof(a), "%s", msg->event.setmode.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_SETOWNER:
        ename = "SETOWNER"; snprintf(a, sizeof(a), "%s", msg->event.setowner.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_SETTIME:
        ename = "SETTIME"; break;
      case ES_EVENT_TYPE_NOTIFY_SETATTRLIST:
        ename = "SETATTRLIST"; snprintf(a, sizeof(a), "%s", msg->event.setattrlist.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_SETFLAGS:
        ename = "SETFLAGS"; snprintf(a, sizeof(a), "%s", msg->event.setflags.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_SETEXTATTR:
        ename = "SETEXTATTR"; snprintf(a, sizeof(a), "%s", msg->event.setextattr.target->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_MMAP:
        ename = "MMAP"; snprintf(a, sizeof(a), "%s flags=0x%x prot=0x%x",
                                 msg->event.mmap.source->path.data,
                                 msg->event.mmap.flags, msg->event.mmap.protection); break;
      case ES_EVENT_TYPE_NOTIFY_EXEC:
        ename = "EXEC"; snprintf(a, sizeof(a), "%s",
                                 msg->event.exec.target->executable->path.data); break;
      case ES_EVENT_TYPE_NOTIFY_FORK:
        ename = "FORK"; snprintf(a, sizeof(a), "child_pid=%d",
                                 audit_token_to_pid(msg->event.fork.child->audit_token)); break;
      default: ename = "OTHER"; break;
    }
    pid_t pid = audit_token_to_pid(msg->process->audit_token);
    const char *exe = msg->process->executable->path.data;
    if (filter && !strstr(exe, filter)) return;
    printf("ES %s pid=%d exe=%s seq=%llu global_seq=%llu a=%s b=%s\n",
           ename, pid, exe, (unsigned long long)msg->seq_num,
           (unsigned long long)msg->global_seq_num, a, b);
    fflush(stdout);
  });

  printf("ES_NEW_CLIENT_RESULT=%d (%s)\n", (int)res, name_of(res));
  fflush(stdout);
  if (res != ES_NEW_CLIENT_RESULT_SUCCESS) return 10;

  es_event_type_t events[] = {
    ES_EVENT_TYPE_NOTIFY_CREATE,    ES_EVENT_TYPE_NOTIFY_WRITE,
    ES_EVENT_TYPE_NOTIFY_UNLINK,    ES_EVENT_TYPE_NOTIFY_RENAME,
    ES_EVENT_TYPE_NOTIFY_LINK,      ES_EVENT_TYPE_NOTIFY_CLONE,
    ES_EVENT_TYPE_NOTIFY_TRUNCATE,  ES_EVENT_TYPE_NOTIFY_SETMODE,
    ES_EVENT_TYPE_NOTIFY_SETOWNER,  ES_EVENT_TYPE_NOTIFY_SETTIME,
    ES_EVENT_TYPE_NOTIFY_SETATTRLIST, ES_EVENT_TYPE_NOTIFY_SETFLAGS,
    ES_EVENT_TYPE_NOTIFY_SETEXTATTR, ES_EVENT_TYPE_NOTIFY_MMAP,
    ES_EVENT_TYPE_NOTIFY_EXEC,      ES_EVENT_TYPE_NOTIFY_FORK,
  };
  es_return_t sr = es_subscribe(client, events, sizeof(events) / sizeof(events[0]));
  printf("ES_SUBSCRIBE=%d (%s)\n", (int)sr, sr == ES_RETURN_SUCCESS ? "SUCCESS" : "ERROR");
  fflush(stdout);
  if (sr != ES_RETURN_SUCCESS) return 11;

  if (seconds > 0) sleep(seconds);
  es_delete_client(client);
  return 0;
}
