package protocol

import (
	"math"
	"strings"
	"testing"

	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protoreflect"
)

func envelope(seq int64) *GameMessage {
	return &GameMessage{ProtocolVersion: proto.Uint32(2), MsgSeq: proto.Int64(seq), GameName: proto.String("Example"), SenderId: proto.Int32(1), ReceiverId: proto.Int32(2)}
}
func roundtrip(t *testing.T, m *GameMessage) *GameMessage {
	t.Helper()
	b, err := proto.Marshal(m)
	if err != nil {
		t.Fatal(err)
	}
	if len(b) > 1200 {
		t.Fatalf("datagram is %d bytes", len(b))
	}
	got := new(GameMessage)
	if err = proto.Unmarshal(b, got); err != nil {
		t.Fatal(err)
	}
	if !proto.Equal(m, got) {
		t.Fatal("roundtrip differs")
	}
	return got
}
func coord(x, y int32) *GameState_Coord {
	c := new(GameState_Coord)
	if x != 0 {
		c.X = proto.Int32(x)
	}
	if y != 0 {
		c.Y = proto.Int32(y)
	}
	return c
}
func largePlayers() *GamePlayers {
	p := new(GamePlayers)
	for i := 0; i < 6; i++ {
		role := NodeRole_NORMAL
		if i == 0 {
			role = NodeRole_MASTER
		}
		if i == 1 {
			role = NodeRole_DEPUTY
		}
		v := &GamePlayer{Id: proto.Int32(math.MaxInt32 - 10 + int32(i)), Name: proto.String(strings.Repeat("n", 24)), Score: proto.Int32(math.MaxInt32), Type: PlayerType_HUMAN.Enum(), Role: role.Enum(), Status: PlayerStatus_VIEWING.Enum()}
		if i > 0 {
			v.IpAddress = proto.String("192.168.255.254")
			v.Port = proto.Int32(65535 - int32(i))
		}
		p.Players = append(p.Players, v)
	}
	return p
}
func maximumEnvelope() *GameMessage {
	m := envelope(math.MaxInt64)
	m.GameName = proto.String(strings.Repeat("g", 32))
	m.SenderId = proto.Int32(math.MaxInt32 - 10)
	m.ReceiverId = proto.Int32(math.MaxInt32 - 9)
	return m
}
func maximumState(t *testing.T) *GameState {
	t.Helper()
	s := &GameState{StateOrder: proto.Int32(math.MaxInt32), NextPlayerId: proto.Int32(math.MaxInt32), Players: largePlayers()}
	occupied := map[[2]int32]bool{}
	occupy := func(x, y int32) {
		t.Helper()
		p := [2]int32{x, y}
		if occupied[p] {
			t.Fatalf("overlapping cell %v", p)
		}
		occupied[p] = true
	}
	for i, h := range [][2]int32{{64, 64}, {66, 64}, {68, 64}, {64, 68}} {
		x, y := h[0], h[1]
		v := &GameState_Snake{PlayerId: proto.Int32(math.MaxInt32 - 10 + int32(i)), HeadDirection: Direction_RIGHT.Enum(), Points: []*GameState_Coord{coord(x, y)}}
		occupy(x, y)
		for edge := 0; edge < 31; edge++ {
			var dx, dy int32
			if edge%2 == 0 {
				dx = -1
			} else {
				dy = -1
			}
			v.Points = append(v.Points, coord(dx, dy))
			x += dx
			y += dy
			occupy(x, y)
		}
		s.Snakes = append(s.Snakes, v)
		s.Players.Players[i].Status = PlayerStatus_PLAYING.Enum()
	}
	for i := int32(0); i < 14; i++ {
		x, y := 65+i%5, 65+i/5
		occupy(x, y)
		s.Foods = append(s.Foods, coord(x, y))
	}
	return s
}
func TestExampleState(t *testing.T) {
	m := envelope(15643)
	m.Type = &GameMessage_State{State: &GameMessage_StateMsg{State: &GameState{StateOrder: proto.Int32(193), NextPlayerId: proto.Int32(3),
		Snakes: []*GameState_Snake{{PlayerId: proto.Int32(1), HeadDirection: Direction_LEFT.Enum(), Points: []*GameState_Coord{coord(5, 1), coord(3, 0), coord(0, 2), coord(-4, 0)}}},
		Players: &GamePlayers{Players: []*GamePlayer{
			{Id: proto.Int32(1), Name: proto.String("Bob"), Score: proto.Int32(8), Role: NodeRole_MASTER.Enum(), Status: PlayerStatus_PLAYING.Enum()},
			{Id: proto.Int32(2), Name: proto.String("Alice"), Score: proto.Int32(0), Role: NodeRole_DEPUTY.Enum(), Status: PlayerStatus_VIEWING.Enum(), IpAddress: proto.String("127.0.0.1"), Port: proto.Int32(12345)},
		}}, Foods: []*GameState_Coord{coord(7, 6), coord(8, 7)},
	}}}
	got := roundtrip(t, m)
	if got.GetState().GetState().GetNextPlayerId() != 3 || got.GetState().GetState().Players.Players[0].GetStatus() != PlayerStatus_PLAYING {
		t.Fatal("example fields")
	}
}
func TestDiscoveryCatalog(t *testing.T) {
	m := envelope(1)
	m.GameName = nil
	m.SenderId = nil
	m.ReceiverId = nil
	m.Type = &GameMessage_Discover{Discover: &GameMessage_DiscoverMsg{}}
	if roundtrip(t, m).GameName != nil {
		t.Fatal("discovery identity")
	}
	m.Type = &GameMessage_Announcement{Announcement: &GameMessage_AnnouncementMsg{Games: []*GameAnnouncement{
		{GameName: proto.String("First"), Config: &GameConfig{}, Players: largePlayers()},
		{GameName: proto.String("Second"), Config: &GameConfig{}, Players: largePlayers()},
	}}}
	got := roundtrip(t, m)
	if got.GameName != nil || len(got.GetAnnouncement().Games) != 2 || got.GetAnnouncement().Games[0].GetGameName() != "First" || got.GetAnnouncement().Games[1].GetGameName() != "Second" {
		t.Fatal("catalog")
	}
}
func TestEmptyCatalog(t *testing.T) {
	m := envelope(3)
	m.GameName = nil
	m.Type = &GameMessage_Announcement{Announcement: &GameMessage_AnnouncementMsg{}}
	if len(roundtrip(t, m).GetAnnouncement().Games) != 0 {
		t.Fatal("nonempty catalog")
	}
}
func TestViewersHoldEveryRole(t *testing.T) {
	for _, role := range []NodeRole{NodeRole_MASTER, NodeRole_DEPUTY, NodeRole_NORMAL} {
		p := &GamePlayer{Id: proto.Int32(1), Name: proto.String("Viewer"), Score: proto.Int32(7), Role: role.Enum(), Status: PlayerStatus_VIEWING.Enum()}
		b, err := proto.Marshal(p)
		if err != nil {
			t.Fatal(err)
		}
		got := new(GamePlayer)
		if err = proto.Unmarshal(b, got); err != nil {
			t.Fatal(err)
		}
		if !proto.Equal(p, got) {
			t.Fatal("viewer roundtrip")
		}
	}
}
func TestViewerJoin(t *testing.T) {
	m := envelope(12)
	m.SenderId = nil
	m.Type = &GameMessage_Join{Join: &GameMessage_JoinMsg{PlayerName: proto.String("Alice"), GameName: proto.String("Example"), RequestedStatus: PlayerStatus_VIEWING.Enum()}}
	got := roundtrip(t, m)
	if got.SenderId != nil || got.GetJoin().GetRequestedStatus() != PlayerStatus_VIEWING || got.GetGameName() != got.GetJoin().GetGameName() {
		t.Fatal("join fields")
	}
}
func TestStatusAndLeaveAreDistinct(t *testing.T) {
	m := envelope(20)
	m.Type = &GameMessage_PlayerStatusChange{PlayerStatusChange: &GameMessage_PlayerStatusChangeMsg{Status: PlayerStatus_VIEWING.Enum()}}
	if roundtrip(t, m).GetPlayerStatusChange() == nil {
		t.Fatal("missing status")
	}
	d := (&GameMessage_PlayerStatusChangeMsg{}).ProtoReflect().Descriptor()
	if d.Fields().ByName("state_order") != nil || !d.ReservedRanges().Has(2) {
		t.Fatal("status schema changed")
	}
	m.Type = &GameMessage_Leave{Leave: &GameMessage_LeaveMsg{}}
	got := roundtrip(t, m)
	if got.GetLeave() == nil || got.GetPlayerStatusChange() != nil {
		t.Fatal("leave overlaps status")
	}
}
func TestErrorRequestIdentity(t *testing.T) {
	m := envelope(400)
	m.Type = &GameMessage_Error{Error: &GameMessage_ErrorMsg{ErrorMessage: proto.String("No space"), RequestSeq: proto.Int64(12)}}
	got := roundtrip(t, m)
	if got.GetMsgSeq() != 400 || got.GetError().GetRequestSeq() != 12 {
		t.Fatal("request sequence")
	}
}
func TestTopologyChange(t *testing.T) {
	m := envelope(24)
	m.Type = &GameMessage_NodeRoleChange{NodeRoleChange: &GameMessage_NodeRoleChangeMsg{ReceiverRole: NodeRole_MASTER.Enum(), StateOrder: proto.Int32(31)}}
	got := roundtrip(t, m)
	if got.GetNodeRoleChange().GetReceiverRole() != NodeRole_MASTER || got.GetNodeRoleChange().GetStateOrder() != 31 || got.GetPlayerStatusChange() != nil {
		t.Fatal("topology")
	}
}
func TestProtocolVersionRequired(t *testing.T) {
	if err := proto.Unmarshal([]byte{8, 1, 18, 0}, new(GameMessage)); err == nil {
		t.Fatal("missing required protocol_version accepted")
	}
}
func TestMaximumPlayingState(t *testing.T) {
	m := maximumEnvelope()
	m.Type = &GameMessage_State{State: &GameMessage_StateMsg{State: maximumState(t)}}
	roundtrip(t, m)
	if proto.Size(m) != 1145 {
		t.Fatalf("maximum state size = %d, want 1145", proto.Size(m))
	}
}
func TestSnakeOwnershipAndSchema(t *testing.T) {
	d := (&GameState_Snake{}).ProtoReflect().Descriptor()
	if d.Fields().ByName("state") != nil || d.Enums().ByName(protoreflect.Name("SnakeState")) != nil || !d.ReservedRanges().Has(3) {
		t.Fatal("snake schema changed")
	}
	s := maximumState(t)
	for _, v := range s.Snakes {
		owners := 0
		for _, p := range s.Players.Players {
			if p.GetId() == v.GetPlayerId() && p.GetStatus() == PlayerStatus_PLAYING {
				owners++
			}
		}
		if owners != 1 {
			t.Fatalf("owners=%d", owners)
		}
	}
}
func TestMaximumSingleGameAnnouncement(t *testing.T) {
	m := maximumEnvelope()
	m.Type = &GameMessage_Announcement{Announcement: &GameMessage_AnnouncementMsg{Games: []*GameAnnouncement{{GameName: proto.String(strings.Repeat("g", 32)), Players: largePlayers(), CanJoin: proto.Bool(true), Config: &GameConfig{Width: proto.Int32(100), Height: proto.Int32(100), FoodStatic: proto.Int32(10), StateDelayMs: proto.Int32(3000)}}}}}
	roundtrip(t, m)
}
