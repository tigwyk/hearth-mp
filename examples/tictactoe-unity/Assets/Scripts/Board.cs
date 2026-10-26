using System;
using Hearth;
using Hearth.Generated;
using UnityEngine;

/// <summary>
/// The tic-tac-toe sample: a 3×3 grid drawn with IMGUI over the typed state that
/// <c>hearth codegen --unity</c> generated into <c>Assets/Hearth</c>.
/// Leave the room field empty to create a room; paste a room name (or pass
/// <c>-room &lt;name&gt;</c>) to join the other player.
/// </summary>
[RequireComponent(typeof(HearthClient))]
public class Board : MonoBehaviour
{
    public string roomType = "tictactoe";
    public string room = "";

    HearthClient client;
    HearthRoom<TicTacToeBoard> joined;
    string status = "";

    void Awake()
    {
        client = GetComponent<HearthClient>();
        var args = Environment.GetCommandLineArgs();
        var i = Array.IndexOf(args, "-room");
        if (i >= 0 && i + 1 < args.Length)
            room = args[i + 1];
    }

    async void Play()
    {
        try
        {
            status = "logging in…";
            await client.LoginAnonymousAsync();
            if (string.IsNullOrEmpty(room))
                room = await client.CreateRoomAsync(roomType);
            status = "joining…";
            joined = await client.JoinAsync<TicTacToeBoard>(roomType, room);
            joined.State.OnStatusChange += _ => status = Describe();
            joined.State.OnTurnChange += _ => status = Describe();
            joined.OnError += (code, message, _) => status = $"{code}: {message}";
            joined.OnReconnecting += (attempt, ms) => status = $"reconnecting in {ms} ms…";
            joined.OnReconnected += () => status = Describe();
            joined.OnDisconnected += (code, reason) => status = $"disconnected ({code})";
            status = Describe();
        }
        catch (Exception e)
        {
            status = e.Message;
        }
    }

    string Describe()
    {
        var s = joined.State.Status;
        switch (s.Variant)
        {
            case TicTacToeStatus.Waiting: return "waiting for a second player";
            case TicTacToeStatus.Won: return s.AsWon == joined.PlayerId ? "you won!" : "you lost";
            case TicTacToeStatus.Draw: return "draw";
            default: return joined.State.Turn == joined.PlayerId ? "your turn" : "their turn";
        }
    }

    void OnGUI()
    {
        GUILayout.BeginArea(new Rect(20, 20, 320, 480));
        if (joined == null)
        {
            GUILayout.Label("Room (empty = create)");
            room = GUILayout.TextField(room);
            if (GUILayout.Button("Play"))
                Play();
        }
        else
        {
            GUILayout.Label("Room: " + room);
            for (var row = 0; row < 3; row++)
            {
                GUILayout.BeginHorizontal();
                for (var col = 0; col < 3; col++)
                {
                    var cell = row * 3 + col;
                    var mark = joined.State.Cells.Count > cell ? joined.State.Cells[cell] : (byte)0;
                    if (GUILayout.Button(mark == 1 ? "X" : mark == 2 ? "O" : " ", GUILayout.Width(80), GUILayout.Height(80)) && mark == 0)
                        joined.Send(TicTacToeIntents.Place((byte)cell));
                }
                GUILayout.EndHorizontal();
            }
            var over = joined.State.Status?.Variant == TicTacToeStatus.Won || joined.State.Status?.Variant == TicTacToeStatus.Draw;
            if (over && GUILayout.Button("Rematch"))
                joined.Send(TicTacToeIntents.Rematch());
        }
        GUILayout.Label(status);
        GUILayout.EndArea();
    }
}
