using System;
using System.Threading.Tasks;
using Hearth;
using Hearth.Generated;
using Newtonsoft.Json.Linq;
using UnityEngine;

/// <summary>
/// The arena prediction sample. Your own avatar is drawn from the predictor
/// (the native plugin runs the same <c>arena_logic::step</c> as the server), so
/// a key press moves it on the next frame. When the server disagrees (another
/// avatar blocks you) the drawn position glides to the corrected prediction at
/// 15% a frame, and snaps only above 2 units. Other avatars come from the
/// snapshot interpolator, so they stay smooth at 20 Hz.
/// </summary>
/// <remarks>
/// Needs <c>wrangler dev</c> in <c>examples/arena</c> and the custom plugin:
/// <c>scripts/build-unity-native.sh --crate examples/arena-unity-plugin --out examples/arena-unity/Assets/Plugins/Hearth</c>.
/// Leave <see cref="room"/> empty to create a room; pass <c>-room &lt;name&gt;</c>
/// (or set the field) to join it from a second window. WASD or arrows move.
/// </remarks>
[RequireComponent(typeof(HearthClient))]
public class Arena : MonoBehaviour
{
    public const string RoomType = "arena";
    public const int TickHz = 20;
    const float SmoothPerFrame = 0.15f;
    const float SnapDistance = 2f;
    const float Pixels = 26f;

    public string room = "";
    public bool connectOnStart = true;
    /// <summary>Tests turn this off and call <see cref="Move"/> themselves.</summary>
    public bool readKeyboard = true;

    public HearthClient Client { get; private set; }
    public HearthRoom<ArenaRoomArena> Room { get; private set; }
    public HearthPredictor<ArenaRoomArena> Predictor { get; private set; }
    public HearthInterpolator Others { get; private set; }

    /// <summary>Where your avatar is drawn: the smoothed prediction.</summary>
    public Vector2 DrawnSelf { get; private set; }

    string status = "starting";
    bool hasDrawn;
    Vector2Int lastDir;

    void Awake()
    {
        Client = GetComponent<HearthClient>();
        Time.fixedDeltaTime = 1f / TickHz;
        var args = Environment.GetCommandLineArgs();
        var i = Array.IndexOf(args, "-room");
        if (i >= 0 && i + 1 < args.Length)
            room = args[i + 1];
    }

    async void Start()
    {
        if (!connectOnStart)
            return;
        try
        {
            await Connect();
        }
        catch (Exception e)
        {
            status = e.Message;
            Debug.LogException(e);
        }
    }

    public async Task Connect()
    {
        status = "logging in";
        await Client.LoginAnonymousAsync();
        if (string.IsNullOrEmpty(room))
        {
            room = await Client.CreateRoomAsync(RoomType);
            Debug.Log($"arena room: {room}  (second window: -room {room})");
        }
        status = "joining " + room;
        Room = await Client.JoinAsync<ArenaRoomArena>(RoomType, room);
        Predictor = new HearthPredictor<ArenaRoomArena>(Room, "arena", TickHz);
        Predictor.OnError += message => status = "prediction off: " + message;
        Others = Room.Interpolator(new object[] { "avatars", "*", "pos" });
        hasDrawn = false;
        status = "joined";
    }

    /// <summary>Send one move: the room intent plus the logic input the predictor steps.</summary>
    public uint Move(Vector2Int dir)
    {
        var d = new JArray(dir.x, dir.y);
        return Predictor.Input(ArenaRoomIntents.Move(d), new JObject { ["dir"] = d.DeepClone() });
    }

    // One intent per tick while a key is held (the server consumes moves each
    // tick, like a held key); releasing sends a single stop.
    void FixedUpdate()
    {
        if (Predictor == null || !readKeyboard)
            return;
        var dir = ReadKeys();
        if (dir == Vector2Int.zero && lastDir == Vector2Int.zero)
            return;
        lastDir = dir;
        Move(dir);
    }

    static Vector2Int ReadKeys()
    {
        var dir = Vector2Int.zero;
        if (Input.GetKey(KeyCode.RightArrow) || Input.GetKey(KeyCode.D)) dir.x += 1;
        if (Input.GetKey(KeyCode.LeftArrow) || Input.GetKey(KeyCode.A)) dir.x -= 1;
        if (Input.GetKey(KeyCode.DownArrow) || Input.GetKey(KeyCode.S)) dir.y += 1;
        if (Input.GetKey(KeyCode.UpArrow) || Input.GetKey(KeyCode.W)) dir.y -= 1;
        return dir;
    }

    void Update()
    {
        if (Predictor == null)
            return;
        var predicted = PosOf(Predictor.State, Room.PlayerId);
        if (predicted == null)
            return;
        if (!hasDrawn)
        {
            DrawnSelf = predicted.Value;
            hasDrawn = true;
        }
        else
        {
            var authoritative = PosOf(Room.State, Room.PlayerId);
            DrawnSelf = authoritative != null && Vector2.Distance(DrawnSelf, authoritative.Value) > SnapDistance
                ? predicted.Value
                : Vector2.Lerp(DrawnSelf, predicted.Value, SmoothPerFrame);
        }
    }

    /// <summary>An avatar's position, or null when it is not in the state.</summary>
    public static Vector2? PosOf(ArenaRoomArena state, string playerId)
    {
        if (playerId == null || !state.Avatars.TryGetValue(playerId, out var avatar) || avatar.Pos.Count < 2)
            return null;
        return new Vector2(avatar.Pos[0], avatar.Pos[1]);
    }

    void OnGUI()
    {
        var origin = new Vector2(20, 20);
        GUI.Box(new Rect(origin, new Vector2(20 * Pixels, 20 * Pixels)), GUIContent.none);
        if (Room != null)
        {
            foreach (var coin in Room.State.Coins.Items)
                GUI.Box(new Rect(origin + new Vector2(coin[0], coin[1]) * Pixels - Vector2.one * 3, Vector2.one * 6), GUIContent.none);
            foreach (var id in Room.State.Avatars.Keys)
            {
                if (id == Room.PlayerId || Others.Sample("avatars", id, "pos") == null)
                    continue;
                var pos = Others.SampleVector2("avatars", id, "pos");
                GUI.Box(new Rect(origin + pos * Pixels - Vector2.one * 10, Vector2.one * 20), id.Substring(0, Math.Min(4, id.Length)));
            }
            if (hasDrawn)
                GUI.Box(new Rect(origin + DrawnSelf * Pixels - Vector2.one * 13, Vector2.one * 26), "you");
        }
        GUI.Label(new Rect(origin.x, origin.y + 20 * Pixels + 8, 600, 24), $"{status}   (WASD / arrows move)   room: {room}");
    }
}
